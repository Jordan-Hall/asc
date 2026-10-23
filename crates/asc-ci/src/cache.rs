//! `asc ci cache` and the shared half of `asc ci artifact` (SPEC §12.7, PLAN U75): the manifest
//! format, collecting files, uploading their content into the repository namespace and
//! materialising a manifest again.
//!
//! File content goes through `Objects` as ordinary `File`/`Chunk` objects (FileIds, chunked over
//! 4 MiB), so it dedupes with repository content. A cache or artifact is one
//! [`ActionsManifest`] (postcard) that the server stores under `PUT|GET
//! /actions/v1/<owner>/<repo>/<cache|artifact>/<key>`. Everything that comes back from the server
//! is untrusted: a manifest is validated before any file is written, every object is verified
//! against its id, and no write ever follows a symlinked ancestor.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::codec::{Enveloped, decode_envelope};
use asc_core::content::{Body, CHUNK_THRESHOLD, ChunkAssembler, ingest};
use asc_core::id::FileId;
use asc_core::object::{ChunkList, Kind};
use asc_core::path::{RepoPath, checkout_safe};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use serde::{Deserialize, Serialize};

/// The only manifest version.
pub const MANIFEST_VERSION: u8 = 1;
/// Most entries in one manifest.
pub const MAX_ENTRIES: usize = 200_000;
/// Largest encoded manifest.
pub const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
/// Largest file `save` and `upload` take (its content is held in memory while it is cut).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// Largest sum of entry sizes in one manifest.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Longest symlink target stored or restored.
pub const MAX_LINK_BYTES: usize = 4096;
/// Longest percent-encoded key (a store file name must stay under common 255-byte limits).
pub const MAX_KEY_PCT: usize = 200;
/// Objects held back before one presence query and the uploads that follow.
const BATCH_FILES: usize = 512;
const BATCH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A bad argument or path.
    #[error("{0}")]
    Input(String),
    /// A manifest that fails validation.
    #[error("invalid manifest: {0}")]
    Manifest(String),
    #[error("{path}: {why}")]
    Fs { path: String, why: String },
    #[error("object store: {0}")]
    Objects(String),
    #[error("server: {0}")]
    Api(String),
    /// Content named by a manifest is missing or does not match its id.
    #[error("content of {0} is missing or corrupt")]
    Content(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn fs_err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Fs {
        path: path.display().to_string(),
        why: e.to_string(),
    }
}

fn obj_err(e: impl std::fmt::Display) -> Error {
    Error::Objects(e.to_string())
}

/// Which namespace of the store a key lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    Cache,
    Artifact,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Artifact => "artifact",
        }
    }
}

/// One cache or artifact (SPEC §12.7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsManifest {
    pub version: u8,
    pub entries: Vec<(RepoPath, Kind, FileId, u64)>,
}

impl ActionsManifest {
    /// A manifest of `entries`, ordered and checked.
    pub fn new(mut entries: Vec<(RepoPath, Kind, FileId, u64)>) -> Result<Self> {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let m = Self {
            version: MANIFEST_VERSION,
            entries,
        };
        m.validate()?;
        Ok(m)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let out = postcard::to_allocvec(self).map_err(|e| Error::Manifest(e.to_string()))?;
        if out.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Manifest("manifest is too large".into()));
        }
        Ok(out)
    }

    /// Decodes and validates bytes from the server.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Manifest("manifest is too large".into()));
        }
        let (m, rest): (Self, &[u8]) =
            postcard::take_from_bytes(bytes).map_err(|e| Error::Manifest(e.to_string()))?;
        if !rest.is_empty() {
            return Err(Error::Manifest("trailing bytes".into()));
        }
        m.validate()?;
        Ok(m)
    }

    /// Version 1, at most [`MAX_ENTRIES`] entries, strictly ascending (so unique) paths that are
    /// safe to check out, only file, executable and symlink kinds, bounded sizes, and no entry
    /// beneath another entry.
    pub fn validate(&self) -> Result<()> {
        let bad = |s: String| Err(Error::Manifest(s));
        if self.version != MANIFEST_VERSION {
            return bad(format!("unsupported version {}", self.version));
        }
        if self.entries.len() > MAX_ENTRIES {
            return bad("too many entries".into());
        }
        let mut total = 0u64;
        let mut all: HashSet<&[u8]> = HashSet::with_capacity(self.entries.len());
        for (i, (path, kind, _, size)) in self.entries.iter().enumerate() {
            if i > 0 && self.entries[i - 1].0 >= *path {
                return bad("entries are not strictly ascending".into());
            }
            if !checkout_safe(path) {
                return bad(format!("unsafe path {path}"));
            }
            match kind {
                Kind::File | Kind::Exec => {}
                Kind::Symlink if *size <= MAX_LINK_BYTES as u64 => {}
                _ => return bad(format!("{path}: unsupported kind")),
            }
            total = total
                .checked_add(*size)
                .filter(|t| *t <= MAX_TOTAL_BYTES)
                .ok_or_else(|| Error::Manifest("total size is too large".into()))?;
            all.insert(path.as_bytes());
        }
        for (path, ..) in &self.entries {
            let mut p = path.parent();
            while let Some(dir) = p {
                if all.contains(dir.as_bytes()) {
                    return bad(format!("{path} lies beneath another entry"));
                }
                p = dir.parent();
            }
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.3).sum()
    }
}

// ---------------------------------------------------------------------------------- keys

/// Percent-encodes every byte except `A-Z a-z 0-9 . _ -` (SPEC §12.7), so a key prefix is a
/// listing prefix.
pub fn pct(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A key or restore-key prefix: 1 to 200 encoded bytes, no control characters.
pub fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.chars().any(char::is_control) {
        return Err(Error::Input(
            "a key must be non-empty and free of control characters".into(),
        ));
    }
    if pct(key).len() > MAX_KEY_PCT {
        return Err(Error::Input(format!(
            "a key may be at most {MAX_KEY_PCT} bytes once percent-encoded"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------- api

/// One row of a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedKey {
    pub key: String,
    /// Unix seconds of the last write or access.
    pub at: i64,
    pub size: u64,
}

/// The manifest endpoints of `asc serve` (`/actions/v1/<owner>/<repo>/…`). Implemented over HTTP
/// by the CLI.
#[async_trait]
pub trait ManifestApi: Send + Sync {
    /// Writes the manifest. `Ok(false)` means the key already exists and `overwrite` was not
    /// allowed (keys are immutable unless an artifact is overwritten on request).
    async fn put(&self, store: StoreKind, key: &str, body: Bytes, overwrite: bool) -> Result<bool>;
    /// The caller's scope, then `trusted`.
    async fn get(&self, store: StoreKind, key: &str) -> Result<Option<Bytes>>;
    /// The newest manifest whose key starts with `prefix`: its key and bytes.
    async fn find_prefix(&self, store: StoreKind, prefix: &str) -> Result<Option<(String, Bytes)>>;
    /// Keys starting with `prefix`, newest first.
    async fn list(&self, store: StoreKind, prefix: &str) -> Result<Vec<ListedKey>>;
}

// ---------------------------------------------------------------------------------- collect

/// One file, directory entry or symlink to store.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: RepoPath,
    pub kind: Kind,
    pub abs: PathBuf,
    pub size: u64,
}

/// What [`collect`] found.
#[derive(Debug, Default)]
pub struct Collected {
    pub items: Vec<Item>,
    /// Entries left out: names a checkout would refuse, non-UTF-8 names on Windows, sockets.
    pub skipped: usize,
}

#[cfg(unix)]
fn os_bytes(s: &std::ffi::OsStr) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Some(s.as_bytes().to_vec())
}

#[cfg(not(unix))]
fn os_bytes(s: &std::ffi::OsStr) -> Option<Vec<u8>> {
    s.to_str().map(|s| s.as_bytes().to_vec())
}

#[cfg(unix)]
fn os_from_bytes(b: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(b.to_vec())
}

#[cfg(not(unix))]
fn os_from_bytes(b: &[u8]) -> std::ffi::OsString {
    String::from_utf8_lossy(b).into_owned().into()
}

fn is_exec(m: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        false
    }
}

/// A relative spec as `/`-joined components: `.` dropped, `..`, a drive or an escape refused.
fn spec_components(root: &Path, spec: &str) -> Result<Vec<String>> {
    if spec.is_empty() || spec.contains('\0') {
        return Err(Error::Input("empty or invalid path".into()));
    }
    let given = Path::new(spec);
    let rel: PathBuf = if given.is_absolute() {
        given
            .strip_prefix(root)
            .map_err(|_| Error::Input(format!("{spec}: outside the root {}", root.display())))?
            .to_path_buf()
    } else {
        // `\` separates on Windows only; on unix it is part of a name.
        given.to_path_buf()
    };
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(
                n.to_str()
                    .ok_or_else(|| Error::Input(format!("{spec}: not valid UTF-8")))?
                    .to_owned(),
            ),
            Component::CurDir => {}
            _ => return Err(Error::Input(format!("{spec}: must stay inside the root"))),
        }
    }
    Ok(out)
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// Collects the files named by `specs` below `root` (a path, a directory or a glob). Directories
/// are walked without following symlinks; symlinks are stored as links. No path may pass through
/// a symlink.
pub fn collect(root: &Path, specs: &[String]) -> Result<Collected> {
    let mut found: BTreeMap<RepoPath, Item> = BTreeMap::new();
    let mut skipped = 0usize;
    for spec in specs {
        let comps = spec_components(root, spec)?;
        let lit = comps.iter().take_while(|c| !has_glob(c)).count();
        let (start, rest) = comps.split_at(lit);
        let matcher = if rest.is_empty() {
            None
        } else {
            let pattern = comps.join("/");
            let g = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Input(format!("{spec}: {e}")))?;
            Some(g.compile_matcher())
        };
        let mut abs = root.to_path_buf();
        for (i, c) in start.iter().enumerate() {
            abs.push(c);
            match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.is_dir() => {}
                // The last literal component may be a file or link; a link as an ancestor
                // never is one.
                Ok(_) if i + 1 == start.len() && matcher.is_none() => {}
                Ok(_) => {
                    return Err(Error::Input(format!(
                        "{}: a path component is not a directory",
                        abs.display()
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    abs.clear();
                    break;
                }
                Err(e) => return Err(fs_err(&abs, e)),
            }
        }
        if abs.as_os_str().is_empty() {
            continue; // a missing path is not an error (actions/cache warns)
        }
        let mut w = Walk {
            root,
            matcher: matcher.as_ref(),
            found: &mut found,
            skipped: &mut skipped,
        };
        w.visit(&abs, matcher.is_none())?;
    }
    Ok(Collected {
        items: found.into_values().collect(),
        skipped,
    })
}

struct Walk<'a> {
    root: &'a Path,
    matcher: Option<&'a globset::GlobMatcher>,
    found: &'a mut BTreeMap<RepoPath, Item>,
    skipped: &'a mut usize,
}

impl Walk<'_> {
    fn rel(&self, abs: &Path) -> Option<Vec<u8>> {
        let rel = abs.strip_prefix(self.root).ok()?;
        let mut out = Vec::new();
        for (i, c) in rel.components().enumerate() {
            let Component::Normal(n) = c else {
                return None;
            };
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(&os_bytes(n)?);
        }
        Some(out)
    }

    /// `take` is true once the entry is selected (the walk is below a matched directory).
    fn visit(&mut self, abs: &Path, take: bool) -> Result<()> {
        let meta = std::fs::symlink_metadata(abs).map_err(|e| fs_err(abs, e))?;
        let Some(rel) = self.rel(abs) else {
            *self.skipped += 1;
            return Ok(());
        };
        let take = take
            || self
                .matcher
                .is_some_and(|m| std::str::from_utf8(&rel).is_ok_and(|s| m.is_match(s)));
        if meta.is_dir() {
            let mut names: Vec<_> = std::fs::read_dir(abs)
                .map_err(|e| fs_err(abs, e))?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<std::io::Result<_>>()
                .map_err(|e| fs_err(abs, e))?;
            names.sort();
            for n in names {
                self.visit(&abs.join(n), take)?;
            }
            return Ok(());
        }
        if !take || rel.is_empty() {
            return Ok(());
        }
        let kind = if meta.file_type().is_symlink() {
            Kind::Symlink
        } else if meta.is_file() {
            if is_exec(&meta) {
                Kind::Exec
            } else {
                Kind::File
            }
        } else {
            *self.skipped += 1;
            return Ok(());
        };
        match RepoPath::new(rel) {
            Ok(path) if checkout_safe(&path) => {
                if self.found.len() >= MAX_ENTRIES {
                    return Err(Error::Input(format!("more than {MAX_ENTRIES} files")));
                }
                self.found.insert(
                    path.clone(),
                    Item {
                        path,
                        kind,
                        abs: abs.to_path_buf(),
                        size: meta.len(),
                    },
                );
            }
            _ => *self.skipped += 1,
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------- upload

/// Counts of an upload.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadStats {
    pub files: usize,
    pub bytes: u64,
    /// Objects the repository did not already hold.
    pub new_objects: usize,
}

struct Pending {
    keys: Vec<(ObjKey, Bytes)>,
    bytes: u64,
}

async fn flush(objects: &dyn Objects, p: &mut Pending, stats: &mut UploadStats) -> Result<()> {
    if p.keys.is_empty() {
        return Ok(());
    }
    let mut seen = HashSet::new();
    let batch: Vec<(ObjKey, Bytes)> = std::mem::take(&mut p.keys)
        .into_iter()
        .filter(|(k, _)| seen.insert(*k))
        .collect();
    p.bytes = 0;
    let keys: Vec<ObjKey> = batch.iter().map(|(k, _)| *k).collect();
    let have = objects.has_many(&keys).await.map_err(obj_err)?;
    if have.len() != keys.len() {
        return Err(Error::Objects("short presence answer".into()));
    }
    for ((key, stored), present) in batch.into_iter().zip(have) {
        if !present {
            objects.put(&key, stored).await.map_err(obj_err)?;
            stats.new_objects += 1;
        }
    }
    Ok(())
}

/// Reads the content of `item` (a link's target for a symlink).
fn read_item(item: &Item) -> Result<Vec<u8>> {
    if item.kind == Kind::Symlink {
        let target = std::fs::read_link(&item.abs).map_err(|e| fs_err(&item.abs, e))?;
        let bytes = os_bytes(target.as_os_str())
            .filter(|b| !b.is_empty() && b.len() <= MAX_LINK_BYTES && !b.contains(&0))
            .ok_or_else(|| fs_err(&item.abs, "unusable symlink target"))?;
        return Ok(bytes);
    }
    let mut f = std::fs::File::open(&item.abs).map_err(|e| fs_err(&item.abs, e))?;
    let meta = f.metadata().map_err(|e| fs_err(&item.abs, e))?;
    if !meta.is_file() {
        return Err(fs_err(&item.abs, "not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(fs_err(
            &item.abs,
            format!("larger than {MAX_FILE_BYTES} bytes"),
        ));
    }
    let mut buf = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut f, MAX_FILE_BYTES + 1),
        &mut buf,
    )
    .map_err(|e| fs_err(&item.abs, e))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return Err(fs_err(&item.abs, "grew while being read"));
    }
    Ok(buf)
}

/// Stores the content of `items` and returns the manifest entries. Objects the repository
/// already holds (from earlier caches or from the repository's own files) are not sent again.
pub async fn upload_items(
    objects: &dyn Objects,
    items: &[Item],
) -> Result<(Vec<(RepoPath, Kind, FileId, u64)>, UploadStats)> {
    let mut entries = Vec::with_capacity(items.len());
    let mut stats = UploadStats::default();
    let mut pending = Pending {
        keys: Vec::new(),
        bytes: 0,
    };
    let mut total = 0u64;
    for item in items {
        let data = read_item(item)?;
        total = total.saturating_add(data.len() as u64);
        if total > MAX_TOTAL_BYTES {
            return Err(Error::Input("total size is too large".into()));
        }
        let got = ingest(std::io::Cursor::new(&data)).map_err(|e| fs_err(&item.abs, e))?;
        drop(data);
        match got.body {
            Body::Inline(b) => {
                pending.bytes += b.len() as u64;
                pending
                    .keys
                    .push((ObjKey::File(got.file), encode_stored(&b)));
            }
            Body::Chunked { list, chunks } => {
                for (id, data) in chunks {
                    pending.bytes += data.len() as u64;
                    pending.keys.push((ObjKey::Chunk(id), encode_stored(&data)));
                }
                pending.keys.push((
                    ObjKey::ChunkList(got.file),
                    encode_stored(&list.to_envelope()),
                ));
            }
        }
        stats.files += 1;
        stats.bytes += got.size;
        entries.push((item.path.clone(), item.kind, got.file, got.size));
        if pending.keys.len() >= BATCH_FILES || pending.bytes >= BATCH_BYTES {
            flush(objects, &mut pending, &mut stats).await?;
        }
    }
    flush(objects, &mut pending, &mut stats).await?;
    Ok((entries, stats))
}

// ---------------------------------------------------------------------------------- restore

/// Counts of a materialisation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RestoreStats {
    pub files: usize,
    pub bytes: u64,
}

/// Creates the directories above `path` below `root`, one at a time, and refuses any that is a
/// symlink or a file. Returns the parent directory.
fn prepare_parent(root: &Path, path: &RepoPath) -> Result<PathBuf> {
    let mut cur = root.to_path_buf();
    let comps: Vec<&[u8]> = path.components().collect();
    for c in &comps[..comps.len() - 1] {
        cur.push(os_from_bytes(c));
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(fs_err(&cur, "a path component is not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur).map_err(|e| fs_err(&cur, e))?;
            }
            Err(e) => return Err(fs_err(&cur, e)),
        }
    }
    Ok(cur)
}

/// The verified content of `id` as the writer `out` receives it, chunk by chunk.
async fn stream_content(
    objects: &dyn Objects,
    id: FileId,
    size: u64,
    what: &str,
    out: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<()> {
    let bad = || Error::Content(what.to_owned());
    let io = |e: std::io::Error| Error::Fs {
        path: what.to_owned(),
        why: e.to_string(),
    };
    if size <= CHUNK_THRESHOLD {
        let data = objects
            .get(&ObjKey::File(id))
            .await
            .map_err(obj_err)?
            .ok_or_else(bad)?;
        if data.len() as u64 != size || FileId::of(&data) != id {
            return Err(bad());
        }
        return out(&data).map_err(io);
    }
    let raw = objects
        .get(&ObjKey::ChunkList(id))
        .await
        .map_err(obj_err)?
        .ok_or_else(bad)?;
    let list: ChunkList = decode_envelope(&raw).map_err(|_| bad())?;
    if list.size != size {
        return Err(bad());
    }
    let ids: Vec<_> = list.chunks.iter().map(|c| c.0).collect();
    let mut asm = ChunkAssembler::new(list, id).map_err(|_| bad())?;
    for cid in ids {
        let data = objects
            .get(&ObjKey::Chunk(cid))
            .await
            .map_err(obj_err)?
            .ok_or_else(bad)?;
        asm.push(&data).map_err(|_| bad())?;
        out(&data).map_err(io)?;
    }
    asm.finish().map_err(|_| bad())?;
    Ok(())
}

fn tmp_name() -> String {
    format!(".ascci-{}.tmp", uuid::Uuid::new_v4().simple())
}

/// Writes every entry of `manifest` below `root`. Existing files are replaced; an existing
/// directory in the way is an error. Nothing is written outside `root` and no path passes
/// through a symlink.
pub async fn materialise(
    objects: &dyn Objects,
    root: &Path,
    manifest: &ActionsManifest,
) -> Result<RestoreStats> {
    manifest.validate()?;
    match std::fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(fs_err(root, "not a directory")),
        Err(_) => std::fs::create_dir_all(root).map_err(|e| fs_err(root, e))?,
    }
    let mut stats = RestoreStats::default();
    for (path, kind, id, size) in &manifest.entries {
        let parent = prepare_parent(root, path)?;
        let name = path.components().next_back().unwrap_or_default();
        let target = parent.join(os_from_bytes(name));
        match std::fs::symlink_metadata(&target) {
            Ok(m) if m.is_dir() => return Err(fs_err(&target, "a directory is in the way")),
            _ => {}
        }
        if *kind == Kind::Symlink {
            let mut link = Vec::new();
            stream_content(objects, *id, *size, &path.to_string(), &mut |b| {
                link.extend_from_slice(b);
                Ok(())
            })
            .await?;
            if link.is_empty() || link.contains(&0) {
                return Err(Error::Content(path.to_string()));
            }
            let _ = std::fs::remove_file(&target);
            make_symlink(&os_from_bytes(&link), &target)?;
        } else {
            let tmp = parent.join(tmp_name());
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| fs_err(&tmp, e))?;
            let res = stream_content(objects, *id, *size, &path.to_string(), &mut |b| {
                file.write_all(b)
            })
            .await;
            drop(file);
            let res = res.and_then(|()| set_mode(&tmp, *kind == Kind::Exec));
            if let Err(e) = res {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            if let Err(e) = std::fs::rename(&tmp, &target) {
                let _ = std::fs::remove_file(&tmp);
                return Err(fs_err(&target, e));
            }
        }
        stats.files += 1;
        stats.bytes += size;
    }
    Ok(stats)
}

#[cfg(unix)]
fn set_mode(p: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).map_err(|e| fs_err(p, e))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &std::ffi::OsStr, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|e| fs_err(link, e))
}

#[cfg(windows)]
fn make_symlink(target: &std::ffi::OsStr, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link).map_err(|e| fs_err(link, e))
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_: &std::ffi::OsStr, link: &Path) -> Result<()> {
    Err(fs_err(link, "symlinks are not supported here"))
}

// ---------------------------------------------------------------------------------- cache

/// What `cache save` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    /// The key already holds a cache; keys are immutable and nothing was uploaded.
    Exists,
    /// Nothing matched the paths; nothing was saved.
    Empty,
    Saved {
        entries: usize,
        stats: UploadStats,
    },
}

/// `asc ci cache save <key> <paths>`.
pub async fn save(
    api: &dyn ManifestApi,
    objects: &dyn Objects,
    root: &Path,
    key: &str,
    specs: &[String],
) -> Result<Saved> {
    check_key(key)?;
    if specs.is_empty() {
        return Err(Error::Input("no paths given".into()));
    }
    if api.get(StoreKind::Cache, key).await?.is_some() {
        return Ok(Saved::Exists);
    }
    let found = collect(root, specs)?;
    if found.items.is_empty() {
        return Ok(Saved::Empty);
    }
    let (entries, stats) = upload_items(objects, &found.items).await?;
    let manifest = ActionsManifest::new(entries)?;
    let body = Bytes::from(manifest.encode()?);
    if !api.put(StoreKind::Cache, key, body, false).await? {
        return Ok(Saved::Exists);
    }
    Ok(Saved::Saved {
        entries: manifest.entries.len(),
        stats,
    })
}

/// A cache that was restored.
#[derive(Debug, PartialEq, Eq)]
pub struct Hit {
    /// The key that matched: `key` itself or the newest key with a restore-key prefix.
    pub matched: String,
    pub exact: bool,
    pub stats: RestoreStats,
}

/// `asc ci cache restore <key> [--restore-keys …]`: the exact key first, then each restore key in
/// order as a prefix, newest first (`actions/cache` semantics).
pub async fn restore(
    api: &dyn ManifestApi,
    objects: &dyn Objects,
    root: &Path,
    key: &str,
    restore_keys: &[String],
) -> Result<Option<Hit>> {
    check_key(key)?;
    for k in restore_keys {
        check_key(k)?;
    }
    let (matched, exact, body) = if let Some(b) = api.get(StoreKind::Cache, key).await? {
        (key.to_owned(), true, b)
    } else {
        let mut found = None;
        for prefix in restore_keys {
            if let Some((k, b)) = api.find_prefix(StoreKind::Cache, prefix).await? {
                // The server decides the match; a reply outside the prefix is not trusted.
                if k.starts_with(prefix.as_str()) {
                    found = Some((k, false, b));
                    break;
                }
            }
        }
        match found {
            Some(f) => f,
            None => return Ok(None),
        }
    };
    let manifest = ActionsManifest::decode(&body)?;
    let stats = materialise(objects, root, &manifest).await?;
    Ok(Some(Hit {
        matched,
        exact,
        stats,
    }))
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::collections::HashMap;
    use std::ops::Range;
    use std::sync::Mutex;
    use std::time::SystemTime;

    use bytes::Bytes;
    use asc_store::codec::decode_stored_capped;
    use asc_store::key::{KeyPrefix, ObjKey};
    use asc_store::objects::{Error as ObjError, Objects, Result as ObjResult};
    use futures::stream::{BoxStream, StreamExt};

    use super::*;

    #[derive(Default)]
    pub struct Mem {
        pub map: Mutex<HashMap<ObjKey, Bytes>>,
        pub puts: Mutex<usize>,
    }

    #[async_trait]
    impl Objects for Mem {
        async fn get(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
            let stored = self.map.lock().unwrap().get(key).cloned();
            match stored {
                None => Ok(None),
                Some(s) => decode_stored_capped(&s, u64::MAX)
                    .map(Some)
                    .map_err(|_| ObjError::Corrupt(*key)),
            }
        }
        async fn get_stored(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
            Ok(self.map.lock().unwrap().get(key).cloned())
        }
        async fn get_range(&self, _: &ObjKey, _: Range<u64>) -> ObjResult<Option<Bytes>> {
            Ok(None)
        }
        async fn put(&self, key: &ObjKey, stored: Bytes) -> ObjResult<()> {
            *self.puts.lock().unwrap() += 1;
            self.map.lock().unwrap().insert(*key, stored);
            Ok(())
        }
        async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
            let m = self.map.lock().unwrap();
            Ok(keys.iter().map(|k| m.contains_key(k)).collect())
        }
        async fn delete(&self, _: &ObjKey) -> ObjResult<()> {
            Ok(())
        }
        fn list(&self, _: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
            futures::stream::empty().boxed()
        }
    }

    /// An in-memory server with `actions/cache` prefix semantics and immutable keys.
    #[derive(Default)]
    pub struct MemApi {
        pub data: Mutex<Vec<(StoreKind, String, Bytes, i64)>>,
    }

    #[async_trait]
    impl ManifestApi for MemApi {
        async fn put(&self, s: StoreKind, key: &str, body: Bytes, over: bool) -> Result<bool> {
            let mut d = self.data.lock().unwrap();
            let at = d.len() as i64;
            if let Some(row) = d.iter_mut().find(|r| r.0 == s && r.1 == key) {
                if !over {
                    return Ok(false);
                }
                row.2 = body;
                return Ok(true);
            }
            d.push((s, key.to_owned(), body, at));
            Ok(true)
        }
        async fn get(&self, s: StoreKind, key: &str) -> Result<Option<Bytes>> {
            let d = self.data.lock().unwrap();
            Ok(d.iter()
                .find(|r| r.0 == s && r.1 == key)
                .map(|r| r.2.clone()))
        }
        async fn find_prefix(&self, s: StoreKind, p: &str) -> Result<Option<(String, Bytes)>> {
            let d = self.data.lock().unwrap();
            Ok(d.iter()
                .filter(|r| r.0 == s && r.1.starts_with(p))
                .max_by_key(|r| r.3)
                .map(|r| (r.1.clone(), r.2.clone())))
        }
        async fn list(&self, s: StoreKind, p: &str) -> Result<Vec<ListedKey>> {
            let d = self.data.lock().unwrap();
            let mut v: Vec<_> = d
                .iter()
                .filter(|r| r.0 == s && r.1.starts_with(p))
                .map(|r| ListedKey {
                    key: r.1.clone(),
                    at: r.3,
                    size: r.2.len() as u64,
                })
                .collect();
            v.sort_by_key(|k| std::cmp::Reverse(k.at));
            Ok(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{Mem, MemApi};
    use super::*;

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    fn specs(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn roundtrip_restores_files_and_modes() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "node_modules/a/index.js", b"console.log(1)");
        write(src.path(), "node_modules/b.txt", b"");
        let big = vec![7u8; 9 * 1024 * 1024];
        write(src.path(), "node_modules/big.bin", &big);
        let (api, objs) = (MemApi::default(), Mem::default());
        let out = save(&api, &objs, src.path(), "k1", &specs(&["node_modules"]))
            .await
            .unwrap();
        assert!(matches!(out, Saved::Saved { entries: 3, .. }), "{out:?}");

        let dst = tempfile::tempdir().unwrap();
        let hit = restore(&api, &objs, dst.path(), "k1", &[])
            .await
            .unwrap()
            .unwrap();
        assert!(hit.exact);
        assert_eq!(hit.stats.files, 3);
        assert_eq!(
            std::fs::read(dst.path().join("node_modules/a/index.js")).unwrap(),
            b"console.log(1)"
        );
        assert_eq!(
            std::fs::read(dst.path().join("node_modules/big.bin")).unwrap(),
            big
        );
        assert!(
            std::fs::read_dir(dst.path().join("node_modules"))
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().contains(".ascci-"))
        );
    }

    #[tokio::test]
    async fn identical_file_in_two_caches_is_stored_once() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "a/same.txt", b"identical content");
        write(src.path(), "b/same.txt", b"identical content");
        let (api, objs) = (MemApi::default(), Mem::default());
        save(&api, &objs, src.path(), "one", &specs(&["a"]))
            .await
            .unwrap();
        let after_one = objs.map.lock().unwrap().len();
        let out = save(&api, &objs, src.path(), "two", &specs(&["b"]))
            .await
            .unwrap();
        assert_eq!(objs.map.lock().unwrap().len(), after_one);
        let Saved::Saved { stats, .. } = out else {
            panic!("{out:?}")
        };
        assert_eq!(stats.new_objects, 0);
        // Two files with the same content inside one cache are one object too.
        write(src.path(), "c/x", b"dup");
        write(src.path(), "c/y", b"dup");
        let before = *objs.puts.lock().unwrap();
        save(&api, &objs, src.path(), "three", &specs(&["c"]))
            .await
            .unwrap();
        assert_eq!(*objs.puts.lock().unwrap() - before, 1);
    }

    #[tokio::test]
    async fn keys_are_immutable() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "f", b"1");
        let (api, objs) = (MemApi::default(), Mem::default());
        assert!(matches!(
            save(&api, &objs, src.path(), "k", &specs(&["f"]))
                .await
                .unwrap(),
            Saved::Saved { .. }
        ));
        write(src.path(), "f", b"2");
        assert_eq!(
            save(&api, &objs, src.path(), "k", &specs(&["f"]))
                .await
                .unwrap(),
            Saved::Exists
        );
        let dst = tempfile::tempdir().unwrap();
        restore(&api, &objs, dst.path(), "k", &[]).await.unwrap();
        assert_eq!(std::fs::read(dst.path().join("f")).unwrap(), b"1");
    }

    #[tokio::test]
    async fn restore_keys_pick_the_newest_prefix_match() {
        let src = tempfile::tempdir().unwrap();
        let (api, objs) = (MemApi::default(), Mem::default());
        for (k, v) in [("os-1", "old"), ("os-2", "new"), ("other-3", "x")] {
            write(src.path(), "f", v.as_bytes());
            save(&api, &objs, src.path(), k, &specs(&["f"]))
                .await
                .unwrap();
        }
        let dst = tempfile::tempdir().unwrap();
        let hit = restore(
            &api,
            &objs,
            dst.path(),
            "os-9",
            &specs(&["nomatch-", "os-"]),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!((hit.matched.as_str(), hit.exact), ("os-2", false));
        assert_eq!(std::fs::read(dst.path().join("f")).unwrap(), b"new");
        assert!(
            restore(&api, &objs, dst.path(), "zz", &specs(&["q-"]))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn globs_select_files() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "pkg/a/node_modules/x.js", b"1");
        write(src.path(), "pkg/b/node_modules/y.js", b"2");
        write(src.path(), "pkg/b/src/z.js", b"3");
        let got = collect(src.path(), &specs(&["pkg/*/node_modules"])).unwrap();
        let paths: Vec<String> = got.items.iter().map(|i| i.path.to_string()).collect();
        assert_eq!(
            paths,
            ["pkg/a/node_modules/x.js", "pkg/b/node_modules/y.js"]
        );
        let got = collect(src.path(), &specs(&["pkg/**/*.js", "missing"])).unwrap();
        assert_eq!(got.items.len(), 3);
    }

    #[test]
    fn paths_must_stay_inside_the_root() {
        let src = tempfile::tempdir().unwrap();
        assert!(collect(src.path(), &specs(&["../x"])).is_err());
        assert!(collect(src.path(), &specs(&["a/../../x"])).is_err());
        let outside = tempfile::tempdir().unwrap();
        let abs = outside.path().join("f");
        assert!(collect(src.path(), &[abs.display().to_string()]).is_err());
        write(src.path(), "in/f", b"x");
        let abs = src.path().join("in");
        assert_eq!(
            collect(src.path(), &[abs.display().to_string()])
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_stored_as_links_and_ancestors_are_not_followed() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "d/real", b"r");
        std::os::unix::fs::symlink("real", src.path().join("d/link")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), src.path().join("esc")).unwrap();
        // A symlinked ancestor of a spec is refused, not walked.
        assert!(collect(src.path(), &specs(&["esc/x"])).is_err());
        let (api, objs) = (MemApi::default(), Mem::default());
        save(&api, &objs, src.path(), "k", &specs(&["d"]))
            .await
            .unwrap();
        let dst = tempfile::tempdir().unwrap();
        restore(&api, &objs, dst.path(), "k", &[]).await.unwrap();
        assert_eq!(
            std::fs::read_link(dst.path().join("d/link"))
                .unwrap()
                .to_str(),
            Some("real")
        );

        // Restoring into a tree whose directory is a symlink writes nothing through it.
        let dst2 = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dst2.path().join("d")).unwrap();
        assert!(restore(&api, &objs, dst2.path(), "k", &[]).await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn corrupt_or_missing_content_is_refused() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "f", b"payload");
        let (api, objs) = (MemApi::default(), Mem::default());
        save(&api, &objs, src.path(), "k", &specs(&["f"]))
            .await
            .unwrap();
        let key = *objs.map.lock().unwrap().keys().next().unwrap();
        let good = objs.map.lock().unwrap().get(&key).cloned().unwrap();
        // Swapped content that decodes but does not hash to the id.
        objs.map
            .lock()
            .unwrap()
            .insert(key, encode_stored(b"tampered"));
        let dst = tempfile::tempdir().unwrap();
        let e = restore(&api, &objs, dst.path(), "k", &[])
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Content(_)), "{e}");
        assert!(!dst.path().join("f").exists());
        assert_eq!(
            std::fs::read_dir(dst.path()).unwrap().count(),
            0,
            "no temp file left"
        );
        objs.map.lock().unwrap().remove(&key);
        assert!(restore(&api, &objs, dst.path(), "k", &[]).await.is_err());
        objs.map.lock().unwrap().insert(key, good);
        assert!(
            restore(&api, &objs, dst.path(), "k", &[])
                .await
                .unwrap()
                .is_some()
        );
    }

    fn entry(p: &str, k: Kind) -> (RepoPath, Kind, FileId, u64) {
        (RepoPath::new(p).unwrap(), k, FileId::of(b""), 0)
    }

    #[test]
    fn manifests_from_a_server_are_validated() {
        let ok =
            ActionsManifest::new(vec![entry("a", Kind::File), entry("b/c", Kind::Exec)]).unwrap();
        assert_eq!(ActionsManifest::decode(&ok.encode().unwrap()).unwrap(), ok);

        let raw = |m: &ActionsManifest| postcard::to_allocvec(m).unwrap();
        let mut m = ok.clone();
        m.version = 2;
        assert!(ActionsManifest::decode(&raw(&m)).is_err());
        let mut m = ok.clone();
        m.entries.reverse();
        assert!(ActionsManifest::decode(&raw(&m)).is_err(), "unsorted");
        let mut m = ok.clone();
        m.entries.push(m.entries[0].clone());
        assert!(ActionsManifest::decode(&raw(&m)).is_err(), "duplicate");
        assert!(
            ActionsManifest::new(vec![entry("a", Kind::File), entry("a/b", Kind::File)]).is_err()
        );
        assert!(ActionsManifest::new(vec![entry("a", Kind::Dir)]).is_err());
        // A name a checkout refuses (a RepoPath alone accepts the backslash).
        assert!(ActionsManifest::new(vec![entry("a\\b", Kind::File)]).is_err());
        let mut bytes = raw(&ok);
        bytes.push(0);
        assert!(ActionsManifest::decode(&bytes).is_err(), "trailing");
        assert!(ActionsManifest::decode(&[0xff; 64]).is_err());
        let mut big = ok.clone();
        big.entries[0].3 = MAX_TOTAL_BYTES;
        big.entries[1].3 = 1;
        assert!(ActionsManifest::decode(&raw(&big)).is_err(), "total size");
    }

    #[test]
    fn manifest_layout_is_pinned() {
        // SPEC §12.7: postcard of { version: u8, entries: Vec<(RepoPath, Kind, FileId, u64)> }.
        let m = ActionsManifest {
            version: 1,
            entries: vec![(
                RepoPath::new("a").unwrap(),
                Kind::Exec,
                FileId::from_bytes([9; 32]),
                5,
            )],
        };
        let mut want = vec![1u8, 1, 1, b'a', 1];
        want.extend([9u8; 32]);
        want.push(5);
        assert_eq!(postcard::to_allocvec(&m).unwrap(), want);
    }

    #[test]
    fn key_encoding_and_limits() {
        assert_eq!(pct("os-linux_1.0"), "os-linux_1.0");
        assert_eq!(pct("a/b c%"), "a%2Fb%20c%25");
        assert_eq!(pct("é"), "%C3%A9");
        assert!(check_key("").is_err());
        assert!(check_key("a\nb").is_err());
        assert!(check_key(&"k".repeat(200)).is_ok());
        assert!(check_key(&"k".repeat(201)).is_err());
        assert!(check_key(&"é".repeat(34)).is_err());
    }

    #[tokio::test]
    async fn nothing_matched_saves_nothing() {
        let src = tempfile::tempdir().unwrap();
        let (api, objs) = (MemApi::default(), Mem::default());
        assert_eq!(
            save(&api, &objs, src.path(), "k", &specs(&["missing"]))
                .await
                .unwrap(),
            Saved::Empty
        );
        assert!(api.data.lock().unwrap().is_empty());
    }
}
