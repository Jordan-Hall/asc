//! Blobs, action entries, storing outputs and restoring them.
//!
//! A blob is `cas/<ab>/<hex>`, or `cas/<ab>/<hex>.zst` when it is kept compressed. A store on a
//! volume that cannot reflink keeps a blob of 4 KiB or more compressed (zstd level 3, the content
//! size in the frame header) when that saves at least a tenth of it: there a restore is a copy
//! either way, so decompressing while copying costs little and the store shrinks. On a volume
//! that reflinks, blobs stay plain so a restore shares their blocks (`asc.buildcache.compress`).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use asc_core::id::FileHasher;
use serde::{Deserialize, Serialize};

use crate::events::{Event, EventKind};
use crate::key::ActionKey;
use crate::{hex, now, unhex};

/// What an output is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    File,
    Exec,
    Link,
}

/// One output file, relative to the directory the action ran in (`/`-separated).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub path: String,
    pub kind: OutputKind,
    pub size: u64,
    /// The blob (`FileId`) in hex; a link's blob is its target text.
    pub blob: String,
}

/// Where an entry was written from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The work tree root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The workflow or CI job (`ASC_WORKFLOW`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
}

/// One cached action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub tool: String,
    /// A label for people: the crate, the project, the task.
    #[serde(default)]
    pub label: String,
    pub outputs: Vec<Output>,
    #[serde(default)]
    pub exit: i32,
    /// Captured output to replay, as blobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub origin: Origin,
}

impl Action {
    pub fn new(tool: &str, label: &str, origin: Origin) -> Self {
        Action {
            tool: tool.to_owned(),
            label: label.to_owned(),
            outputs: Vec::new(),
            exit: 0,
            stdout: None,
            stderr: None,
            created: now(),
            origin,
        }
    }

    /// The bytes of every output.
    pub fn size(&self) -> u64 {
        self.outputs.iter().map(|o| o.size).sum()
    }
}

/// An entry with its key and last use, as `ls` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub key: ActionKey,
    pub action: Action,
    pub last_use: u64,
    /// The entry file's own size.
    pub entry_bytes: u64,
}

/// The store at one root, with read-only stores below it.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    /// Stores read on a miss and never written (`asc.buildcache.lower`): a host cache mounted
    /// read-only into a job, filled by a trusted build.
    lower: Vec<PathBuf>,
    /// New blobs are written compressed when that saves space.
    compress: bool,
}

/// When blobs are kept compressed (`asc.buildcache.compress`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compress {
    /// Only on a volume that cannot reflink (the default).
    #[default]
    Auto,
    Always,
    Never,
}

impl Compress {
    /// `auto`, `true` or `false` (git's boolean spellings too).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "true" | "yes" | "on" | "1" => Some(Self::Always),
            "false" | "no" | "off" | "0" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Blobs smaller than this stay plain: the saving is a few hundred bytes at most.
pub const COMPRESS_MIN: u64 = 4096;
const ZSTD_LEVEL: i32 = 3;

/// The compressed form of the blob at `raw`.
fn zst_of(raw: &Path) -> PathBuf {
    let mut p = raw.as_os_str().to_owned();
    p.push(".zst");
    PathBuf::from(p)
}

/// The uncompressed size a `.zst` blob records in its frame header.
fn zst_content_size(path: &Path) -> io::Result<u64> {
    let mut head = [0u8; 18];
    let mut f = fs::File::open(path)?;
    let mut n = 0;
    while n < head.len() {
        let got = f.read(&mut head[n..])?;
        if got == 0 {
            break;
        }
        n += got;
    }
    match zstd::zstd_safe::get_frame_content_size(&head[..n]) {
        Ok(Some(size)) => Ok(size),
        _ => Err(bad("a compressed blob has no content size")),
    }
}

/// Opens the blob `id` of the `cas` directory `cas`, plain or compressed, and its size.
fn open_in(cas: &Path, id: &[u8; 32]) -> io::Result<(Box<dyn Read>, u64)> {
    let raw = sharded(cas, &hex(id));
    match fs::File::open(&raw) {
        Ok(f) => {
            let len = f.metadata()?.len();
            Ok((Box::new(f), len))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let zst = zst_of(&raw);
            let len = zst_content_size(&zst)?;
            let d = zstd::Decoder::new(fs::File::open(&zst)?)?;
            Ok((Box::new(d.take(len)), len))
        }
        Err(e) => Err(e),
    }
}

/// Whether `dir` can reflink: answered once per store and kept in `<root>/volume`.
fn reflinks(root: &Path) -> bool {
    let mark = root.join("volume");
    match fs::read_to_string(&mark).as_deref().map(str::trim) {
        Ok("reflink") => return true,
        Ok("copy") => return false,
        _ => {}
    }
    let probe = root
        .join("tmp")
        .join(format!("probe-{}", uuid::Uuid::new_v4().simple()));
    let copy = zst_of(&probe);
    let ok = fs::write(&probe, b"asc reflink probe").is_ok()
        && reflink_copy::reflink(&probe, &copy).is_ok();
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(&copy);
    let _ = fs::write(&mark, if ok { "reflink\n" } else { "copy\n" });
    ok
}

fn bad(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}

/// A relative output path: `/`-separated normal components only.
pub(crate) fn check_rel(path: &str) -> io::Result<()> {
    let ok = !path.is_empty()
        && !path.contains('\\')
        && !path.contains(':')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..");
    if ok {
        Ok(())
    } else {
        Err(bad(format!("'{path}' is not a relative output path")))
    }
}

/// A link target that stays inside the output root: relative, `/`-separated, and climbing only
/// in a leading run of `..` that ends at or below the root. After the first named component it
/// only descends, so it never climbs back out through a link it passes (`up -> .` then
/// `m -> up/..`), whatever the file system's case or Unicode rules make of the names.
pub(crate) fn check_link(path: &str, target: &str) -> io::Result<()> {
    let refuse = || {
        bad(format!(
            "link '{path}' points outside the outputs ('{target}')"
        ))
    };
    if target.is_empty()
        || target.starts_with('/')
        || target.contains('\\')
        || target.contains(':')
        || target.contains('\0')
    {
        return Err(refuse());
    }
    let mut depth = path.split('/').count() - 1;
    let mut named = false;
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." if !named => depth = depth.checked_sub(1).ok_or_else(refuse)?,
            ".." => return Err(refuse()),
            _ => named = true,
        }
    }
    Ok(())
}
/// Output paths that can be written one by one without one writing through another: each is a
/// relative path, none repeats, and none lies below another output (a link would carry it out).
pub(crate) fn check_paths(action: &Action) -> io::Result<()> {
    let mut paths: Vec<&str> = action.outputs.iter().map(|o| o.path.as_str()).collect();
    for p in &paths {
        check_rel(p)?;
    }
    paths.sort_unstable();
    for w in paths.windows(2) {
        if w[1] == w[0] || w[1].strip_prefix(w[0]).is_some_and(|r| r.starts_with('/')) {
            return Err(bad(format!(
                "output '{}' lies below output '{}'",
                w[1], w[0]
            )));
        }
    }
    Ok(())
}

fn sharded(dir: &Path, name: &str) -> PathBuf {
    dir.join(&name[..2]).join(name)
}

impl Store {
    /// Opens (creating) the store at `root`, normally `<ASC_HOME>/buildcache`.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        for d in ["cas", "ac", "events", "tmp"] {
            fs::create_dir_all(root.join(d))?;
        }
        let compress = !reflinks(&root);
        Ok(Store {
            root,
            lower: Vec::new(),
            compress,
        })
    }

    /// The same store with the compression rule `mode` for new blobs. Blobs already stored
    /// keep their form; reads take either.
    pub fn with_compress(mut self, mode: Compress) -> Self {
        self.compress = match mode {
            Compress::Auto => !reflinks(&self.root),
            Compress::Always => true,
            Compress::Never => false,
        };
        self
    }

    /// Whether new blobs are written compressed (when that saves space).
    pub fn compresses(&self) -> bool {
        self.compress
    }

    /// The same store, reading `dirs` in order on a miss. A lower store is only read: an entry
    /// found there is copied up, each blob checked against its id, so this store never refers
    /// to a file it does not hold and a damaged lower store costs a miss, not a wrong output.
    pub fn with_lower(mut self, dirs: Vec<PathBuf>) -> Self {
        self.lower = dirs.into_iter().filter(|d| *d != self.root).collect();
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The read-only stores below this one.
    pub fn lower(&self) -> &[PathBuf] {
        &self.lower
    }

    pub(crate) fn cas(&self) -> PathBuf {
        self.root.join("cas")
    }

    pub(crate) fn ac(&self) -> PathBuf {
        self.root.join("ac")
    }

    pub(crate) fn events_dir(&self) -> PathBuf {
        self.root.join("events")
    }

    fn tmp_file(&self) -> PathBuf {
        self.root
            .join("tmp")
            .join(uuid::Uuid::new_v4().simple().to_string())
    }

    /// Where the plain form of blob `id` is (or would be).
    pub fn blob_path(&self, id: &[u8; 32]) -> PathBuf {
        sharded(&self.cas(), &hex(id))
    }

    /// The file holding blob `id`, and whether it is compressed.
    pub fn blob_file(&self, id: &[u8; 32]) -> Option<(PathBuf, bool)> {
        let raw = self.blob_path(id);
        if raw.is_file() {
            return Some((raw, false));
        }
        let zst = zst_of(&raw);
        zst.is_file().then_some((zst, true))
    }

    /// Opens blob `id` for reading its plain bytes, and its plain size.
    pub fn open_blob(&self, id: &[u8; 32]) -> io::Result<(Box<dyn Read>, u64)> {
        open_in(&self.cas(), id)
    }

    /// A plain file holding blob `id`, for a reader that wants a path (Go's `DiskPath`, a
    /// streamed HTTP body): a compressed blob is stored plain from then on. `None` when it is
    /// gone.
    pub fn plain_blob_path(&self, id: &[u8; 32]) -> io::Result<Option<PathBuf>> {
        let raw = self.blob_path(id);
        if raw.is_file() {
            return Ok(Some(raw));
        }
        let zst = zst_of(&raw);
        let (mut r, len) = match open_in(&self.cas(), id) {
            Ok(x) => x,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let tmp = self.tmp_file();
        let written = (|| -> io::Result<u64> {
            let mut out = fs::File::create(&tmp)?;
            let n = io::copy(&mut r, &mut out)?;
            out.sync_data()?;
            Ok(n)
        })();
        match written {
            Ok(n) if n == len => {}
            Ok(_) => {
                let _ = fs::remove_file(&tmp);
                return Err(bad("a compressed blob is shorter than its header says"));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
        set_readonly(&tmp);
        Self::place(&tmp, &raw, false)?;
        let _ = remove_readonly(&zst);
        Ok(Some(raw))
    }

    pub fn entry_path(&self, key: &ActionKey) -> PathBuf {
        sharded(&self.ac(), &key.to_string())
    }

    /// Moves `tmp` to `dest` unless `dest` exists (same content by name).
    fn place(tmp: &Path, dest: &Path, replace: bool) -> io::Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if !replace && dest.exists() {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        match fs::rename(tmp, dest) {
            Ok(()) => Ok(()),
            // Another process placed the same blob first.
            Err(_) if !replace && dest.exists() => {
                let _ = fs::remove_file(tmp);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(tmp);
                Err(e)
            }
        }
    }

    /// Stores what `src` yields, hashing while copying, so a file that changes meanwhile is
    /// stored under the id of the bytes actually copied.
    pub fn put_blob_reader(&self, src: &mut dyn Read) -> io::Result<([u8; 32], u64)> {
        let tmp = self.tmp_file();
        let mut out = fs::File::create(&tmp)?;
        let mut h = FileHasher::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut size = 0u64;
        let copied = (|| -> io::Result<()> {
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                h.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                size += n as u64;
            }
        })();
        if let Err(e) = copied.and_then(|()| out.sync_data()) {
            drop(out);
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        drop(out);
        let id = *h.finalize().as_bytes();
        if self.has_blob(&id) {
            let _ = fs::remove_file(&tmp);
            return Ok((id, size));
        }
        if self.compress && size >= COMPRESS_MIN {
            match self.compressed(&tmp, size) {
                Ok(Some(z)) => {
                    let _ = fs::remove_file(&tmp);
                    set_readonly(&z);
                    Self::place(&z, &zst_of(&self.blob_path(&id)), false)?;
                    return Ok((id, size));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
            }
        }
        set_readonly(&tmp);
        Self::place(&tmp, &self.blob_path(&id), false)?;
        Ok((id, size))
    }

    /// A compressed copy of `tmp` (`size` bytes) when it is at most nine tenths of the size.
    fn compressed(&self, tmp: &Path, size: u64) -> io::Result<Option<PathBuf>> {
        let z = zst_of(tmp);
        let made = (|| -> io::Result<u64> {
            let mut enc = zstd::Encoder::new(fs::File::create(&z)?, ZSTD_LEVEL)?;
            enc.include_contentsize(true)?;
            enc.set_pledged_src_size(Some(size))?;
            io::copy(&mut fs::File::open(tmp)?, &mut enc)?;
            let f = enc.finish()?;
            f.sync_data()?;
            Ok(f.metadata()?.len())
        })();
        match made {
            Ok(n) if n.saturating_mul(10) <= size.saturating_mul(9) => Ok(Some(z)),
            Ok(_) => {
                let _ = fs::remove_file(&z);
                Ok(None)
            }
            Err(e) => {
                let _ = fs::remove_file(&z);
                Err(e)
            }
        }
    }

    pub fn put_blob(&self, bytes: &[u8]) -> io::Result<([u8; 32], u64)> {
        self.put_blob_reader(&mut &bytes[..])
    }

    pub fn read_blob(&self, id: &[u8; 32]) -> io::Result<Vec<u8>> {
        let (mut r, len) = self.open_blob(id)?;
        let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        r.read_to_end(&mut out)?;
        Ok(out)
    }

    pub fn has_blob(&self, id: &[u8; 32]) -> bool {
        self.blob_file(id).is_some()
    }

    /// Stores the files at `rels` below `base` (a directory is every file below it, links not
    /// followed) and returns them as outputs, sorted by path. A missing path is an error.
    pub fn put_outputs(&self, base: &Path, rels: &[String]) -> io::Result<Vec<Output>> {
        let mut files: Vec<String> = Vec::new();
        for rel in rels {
            check_rel(rel)?;
            collect(base, rel, &mut files)?;
        }
        files.sort();
        files.dedup();
        let mut out = Vec::with_capacity(files.len());
        for rel in files {
            let abs = base.join(&rel);
            let meta = fs::symlink_metadata(&abs)?;
            let (kind, (id, size)) = if meta.file_type().is_symlink() {
                let target = fs::read_link(&abs)?;
                let text = target.to_string_lossy().replace('\\', "/");
                (OutputKind::Link, self.put_blob(text.as_bytes())?)
            } else {
                let mut f = fs::File::open(&abs)?;
                let kind = if executable(&meta) {
                    OutputKind::Exec
                } else {
                    OutputKind::File
                };
                (kind, self.put_blob_reader(&mut f)?)
            };
            out.push(Output {
                path: rel,
                kind,
                size,
                blob: hex(&id),
            });
        }
        Ok(out)
    }

    /// Writes the entry after its blobs exist.
    pub fn put_action(&self, key: &ActionKey, action: &Action) -> io::Result<()> {
        check_paths(action)?;
        for blob in action
            .outputs
            .iter()
            .map(|o| o.blob.as_str())
            .chain(action.stdout.as_deref())
            .chain(action.stderr.as_deref())
        {
            let id = unhex(blob).ok_or_else(|| bad("a blob id is not 64 hex digits"))?;
            if !self.has_blob(&id) {
                return Err(bad(format!("blob {blob} is not in the store")));
            }
        }
        for o in action.outputs.iter().filter(|o| o.kind == OutputKind::Link) {
            check_link(&o.path, &self.link_target(&o.blob)?)?;
        }
        let tmp = self.tmp_file();
        let body = serde_json::to_vec(action).map_err(|e| bad(e.to_string()))?;
        fs::write(&tmp, body)?;
        Self::place(&tmp, &self.entry_path(key), true)?;
        self.record(&Event::new(
            EventKind::Put,
            &action.tool,
            key,
            &action.origin,
            action.size(),
        ));
        Ok(())
    }

    /// The entry, if present and readable; marks it used. A miss reads the lower stores.
    pub fn get_action(&self, key: &ActionKey) -> io::Result<Option<Action>> {
        let path = self.entry_path(key);
        let body = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => match self.copy_up(key)? {
                Some(b) => b,
                None => return Ok(None),
            },
            Err(e) => return Err(e),
        };
        let Ok(action) = serde_json::from_slice::<Action>(&body) else {
            // A torn or foreign file is a miss, and is taken out.
            let _ = fs::remove_file(&path);
            return Ok(None);
        };
        if let Ok(f) = fs::File::options().write(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        Ok(Some(action))
    }

    /// Copies the entry `key` and its blobs from the first lower store that holds all of them
    /// whole, and returns the entry as written here. A lower store is never written: its files
    /// keep their times, so a backup of it sees no change.
    fn copy_up(&self, key: &ActionKey) -> io::Result<Option<Vec<u8>>> {
        let name = key.to_string();
        for root in &self.lower {
            let Ok(body) = fs::read(sharded(&root.join("ac"), &name)) else {
                continue;
            };
            let Ok(action) = serde_json::from_slice::<Action>(&body) else {
                continue;
            };
            if check_paths(&action).is_err() || !self.copy_blobs(root, &action)? {
                continue;
            }
            if self.put_action(key, &action).is_err() {
                continue;
            }
            return fs::read(self.entry_path(key)).map(Some);
        }
        Ok(None)
    }

    /// Copies the blobs of `action` this store lacks from the lower store at `root`. `false`
    /// when one is missing there or its bytes are not those of its id.
    fn copy_blobs(&self, root: &Path, action: &Action) -> io::Result<bool> {
        let cas = root.join("cas");
        for blob in action
            .outputs
            .iter()
            .map(|o| o.blob.as_str())
            .chain(action.stdout.as_deref())
            .chain(action.stderr.as_deref())
        {
            let Some(id) = unhex(blob) else {
                return Ok(false);
            };
            if self.has_blob(&id) {
                continue;
            }
            let Ok((mut f, _)) = open_in(&cas, &id) else {
                return Ok(false);
            };
            // Placed under the id of the bytes read: a damaged blob lands under another name and
            // is taken by the next gc, never under the id the entry names. A damaged compressed
            // blob fails to decode: a miss.
            let Ok((got, _)) = self.put_blob_reader(&mut f) else {
                return Ok(false);
            };
            if got != id {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Looks `key` up and records the hit or miss for `tool`.
    pub fn lookup(
        &self,
        tool: &str,
        key: &ActionKey,
        origin: &Origin,
    ) -> io::Result<Option<Action>> {
        let got = self.get_action(key)?;
        let kind = if got.is_some() {
            EventKind::Hit
        } else {
            EventKind::Miss
        };
        let size = got.as_ref().map_or(0, Action::size);
        self.record(&Event::new(kind, tool, key, origin, size));
        Ok(got)
    }

    /// Writes the outputs of `action` below `base`. `Ok(false)` when a blob is gone (evicted
    /// meanwhile): the caller runs the tool as on a miss. Each file is a copy (a clone where the
    /// file system has them), never a link into the store.
    pub fn restore(&self, action: &Action, base: &Path) -> io::Result<bool> {
        if !self.restorable(action)? {
            return Ok(false);
        }
        for o in &action.outputs {
            if !self.restore_one(o, base)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Checks the paths and links of `action`; `Ok(false)` when one of its blobs is gone.
    pub(crate) fn restorable(&self, action: &Action) -> io::Result<bool> {
        check_paths(action)?;
        for o in &action.outputs {
            let id = unhex(&o.blob).ok_or_else(|| bad("a blob id is not 64 hex digits"))?;
            if !self.has_blob(&id) {
                return Ok(false);
            }
            if o.kind == OutputKind::Link {
                check_link(&o.path, &self.link_target(&o.blob)?)?;
            }
        }
        Ok(true)
    }

    /// Writes one output of an entry that passed [`Store::restorable`]. `Ok(false)` when its
    /// blob was evicted meanwhile.
    pub(crate) fn restore_one(&self, o: &Output, base: &Path) -> io::Result<bool> {
        let id = unhex(&o.blob).ok_or_else(|| bad("a blob id is not 64 hex digits"))?;
        let dest = base.join(&o.path);
        // A directory on the way that is a link (left in the tree, or made by an earlier
        // output) would carry the write outside `base`.
        let mut dir = base.to_path_buf();
        for c in o
            .path
            .split('/')
            .collect::<Vec<_>>()
            .split_last()
            .map_or(&[][..], |x| x.1)
        {
            dir.push(c);
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(bad(format!("'{}' is below a link", o.path)));
                }
                _ => {}
            }
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = dest.with_file_name(format!(".{name}.asc-{}", uuid::Uuid::new_v4().simple()));
        let result = (|| -> io::Result<()> {
            if o.kind == OutputKind::Link {
                let target = self.link_target(&o.blob)?;
                make_link(&target, &tmp)?;
            } else {
                match self.blob_file(&id) {
                    Some((path, false)) => {
                        reflink_copy::reflink_or_copy(path, &tmp)?;
                    }
                    Some((_, true)) => {
                        let (mut r, len) = self.open_blob(&id)?;
                        let mut out = fs::File::create(&tmp)?;
                        if io::copy(&mut r, &mut out)? != len {
                            return Err(bad("a compressed blob is shorter than its header says"));
                        }
                    }
                    None => return Err(io::ErrorKind::NotFound.into()),
                }
                set_mode(&tmp, o.kind == OutputKind::Exec)?;
            }
            if fs::symlink_metadata(&dest).is_ok_and(|m| m.is_dir()) {
                return Err(bad(format!("'{}' is a directory", o.path)));
            }
            fs::rename(&tmp, &dest)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            if e.kind() == io::ErrorKind::NotFound && !self.has_blob(&id) {
                return Ok(false);
            }
            return Err(e);
        }
        Ok(true)
    }

    /// The target text of a link output; a link blob is at most 4 KiB of UTF-8.
    pub(crate) fn link_target(&self, blob: &str) -> io::Result<String> {
        let id = unhex(blob).ok_or_else(|| bad("a blob id is not 64 hex digits"))?;
        let mut text = String::new();
        self.open_blob(&id)?
            .0
            .take(4097)
            .read_to_string(&mut text)
            .map_err(|_| bad("a link target is not UTF-8"))?;
        if text.len() > 4096 {
            return Err(bad("a link target is longer than 4 KiB"));
        }
        Ok(text)
    }

    /// Writes a blob to `w` (replayed stdout or stderr).
    pub fn copy_blob_to(&self, blob: &str, w: &mut dyn Write) -> io::Result<()> {
        let id = unhex(blob).ok_or_else(|| bad("a blob id is not 64 hex digits"))?;
        let (mut f, _) = self.open_blob(&id)?;
        io::copy(&mut f, w)?;
        Ok(())
    }

    /// Removes one entry. Its blobs go at the next `gc`.
    pub fn remove(&self, key: &ActionKey) -> io::Result<bool> {
        match fs::remove_file(self.entry_path(key)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn set_readonly(path: &Path) {
    if let Ok(m) = fs::metadata(path) {
        let mut perms = m.permissions();
        perms.set_readonly(true);
        let _ = fs::set_permissions(path, perms);
    }
}

/// Blobs are read-only; Windows refuses to remove those.
pub(crate) fn remove_readonly(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mut p = fs::metadata(path)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            fs::set_permissions(path, p)?;
            fs::remove_file(path)
        }
        other => other,
    }
}

fn collect(base: &Path, rel: &str, out: &mut Vec<String>) -> io::Result<()> {
    let abs = base.join(rel);
    let meta = fs::symlink_metadata(&abs)?;
    if meta.is_dir() {
        let mut names: Vec<String> = Vec::new();
        for e in fs::read_dir(&abs)? {
            let e = e?;
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| bad(format!("a name below '{rel}' is not UTF-8")))?;
            names.push(name);
        }
        names.sort();
        for n in names {
            collect(base, &format!("{rel}/{n}"), out)?;
        }
    } else {
        out.push(rel.to_owned());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(crate) fn executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_mode(path: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
    )
}

#[cfg(not(unix))]
fn set_mode(path: &Path, _exec: bool) -> io::Result<()> {
    let mut p = fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    p.set_readonly(false);
    fs::set_permissions(path, p)
}

#[cfg(unix)]
fn make_link(target: &str, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn make_link(target: &str, at: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target.replace('/', "\\"), at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("build")).unwrap();
        (d, s)
    }

    /// A lower store holding one entry `key` with the outputs `lib.rlib` and `out.txt`.
    fn lower_with_entry(d: &Path) -> (Store, ActionKey, Action) {
        let lower = Store::open(d.join("host")).unwrap();
        let src = d.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rlib"), "rlib").unwrap();
        fs::write(src.join("out.txt"), "out").unwrap();
        let key = ActionKey::builder("rustc").inputs(&[7; 32]).finish();
        let mut a = Action::new("rustc", "nova", Origin::default());
        a.outputs = lower
            .put_outputs(&src, &["lib.rlib".into(), "out.txt".into()])
            .unwrap();
        lower.put_action(&key, &a).unwrap();
        (lower, key, a)
    }

    fn mtimes(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
        let mut out = Vec::new();
        for e in walk(dir) {
            out.push((e.clone(), fs::metadata(&e).unwrap().modified().unwrap()));
        }
        out.sort();
        out
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn a_miss_copies_the_entry_up_from_a_lower_store_and_leaves_it_untouched() {
        let d = tempfile::tempdir().unwrap();
        let (lower, key, a) = lower_with_entry(d.path());
        let before = mtimes(lower.root());
        std::thread::sleep(std::time::Duration::from_millis(20));

        let s = Store::open(d.path().join("job"))
            .unwrap()
            .with_lower(vec![d.path().join("missing"), lower.root().to_path_buf()]);
        let got = s.get_action(&key).unwrap().unwrap();
        assert_eq!(got, a);
        assert!(s.entry_path(&key).is_file(), "the entry is copied up");
        for o in &a.outputs {
            assert!(
                s.has_blob(&unhex(&o.blob).unwrap()),
                "{} is copied up",
                o.path
            );
        }
        let dst = d.path().join("wt");
        assert!(s.restore(&got, &dst).unwrap());
        assert_eq!(fs::read_to_string(dst.join("lib.rlib")).unwrap(), "rlib");
        assert_eq!(
            mtimes(lower.root()),
            before,
            "the lower store is never written"
        );
    }

    #[test]
    fn a_damaged_lower_blob_is_a_miss() {
        let d = tempfile::tempdir().unwrap();
        let (lower, key, a) = lower_with_entry(d.path());
        let id = unhex(&a.outputs[0].blob).unwrap();
        let path = lower.blob_path(&id);
        let mut perms = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&path, perms).unwrap();
        fs::write(&path, "poisoned").unwrap();

        let s = Store::open(d.path().join("job"))
            .unwrap()
            .with_lower(vec![lower.root().to_path_buf()]);
        assert!(s.get_action(&key).unwrap().is_none());
        assert!(!s.entry_path(&key).exists());
        assert!(!s.has_blob(&id), "the damaged bytes never take the id");
    }

    #[test]
    fn a_lower_entry_with_a_missing_blob_or_an_escaping_path_is_a_miss() {
        let d = tempfile::tempdir().unwrap();
        let (lower, key, a) = lower_with_entry(d.path());
        let id = unhex(&a.outputs[1].blob).unwrap();
        let path = lower.blob_path(&id);
        let mut perms = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&path, perms).unwrap();
        fs::remove_file(&path).unwrap();
        let s = Store::open(d.path().join("job"))
            .unwrap()
            .with_lower(vec![lower.root().to_path_buf()]);
        assert!(s.get_action(&key).unwrap().is_none());

        let evil = ActionKey::builder("rustc").inputs(&[8; 32]).finish();
        let mut bad = a.clone();
        bad.outputs[0].path = "../escape".into();
        let entry = sharded(&lower.ac(), &evil.to_string());
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(s.get_action(&evil).unwrap().is_none());
    }

    #[test]
    fn the_store_itself_is_not_its_own_lower_store() {
        let (_d, s) = store();
        let root = s.root().to_path_buf();
        let s = s.with_lower(vec![root.clone()]);
        assert!(s.lower().is_empty());
    }

    #[test]
    fn check_rel_refuses_escapes() {
        for ok in ["a", "a/b.rlib", "target/debug/x"] {
            check_rel(ok).unwrap();
        }
        for bad in ["", "/a", "a/../b", "./a", "a//b", "C:/x", "a\\b", "a/."] {
            assert!(check_rel(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn blobs_are_content_named_and_shared() {
        let (_d, s) = store();
        let (a, n) = s.put_blob(b"hello").unwrap();
        let (b, _) = s.put_blob(b"hello").unwrap();
        assert_eq!((a, n), (b, 5));
        let mut h = FileHasher::new();
        h.update(b"hello");
        assert_eq!(&a, h.finalize().as_bytes(), "a blob id is the asc FileId");
        assert_eq!(s.read_blob(&a).unwrap(), b"hello");
        assert_eq!(fs::read_dir(s.root().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn outputs_round_trip_into_another_directory() {
        let (d, s) = store();
        let src = d.path().join("wt1");
        fs::create_dir_all(src.join("dist/assets")).unwrap();
        fs::write(src.join("dist/index.html"), "<p>").unwrap();
        fs::write(src.join("dist/assets/app.js"), "x()").unwrap();
        fs::write(src.join("lib.rlib"), "rlib").unwrap();
        let outputs = s
            .put_outputs(&src, &["dist".into(), "lib.rlib".into()])
            .unwrap();
        let paths: Vec<_> = outputs.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths, ["dist/assets/app.js", "dist/index.html", "lib.rlib"]);

        let key = ActionKey::builder("vite").inputs(&[1; 32]).finish();
        let mut a = Action::new("vite", "web", Origin::default());
        a.outputs = outputs;
        s.put_action(&key, &a).unwrap();
        let got = s.get_action(&key).unwrap().unwrap();
        assert_eq!(got, a);

        let dst = d.path().join("wt2");
        fs::create_dir_all(dst.join("dist")).unwrap();
        fs::write(dst.join("dist/index.html"), "stale").unwrap();
        assert!(s.restore(&got, &dst).unwrap());
        assert_eq!(
            fs::read_to_string(dst.join("dist/index.html")).unwrap(),
            "<p>"
        );
        assert_eq!(
            fs::read_to_string(dst.join("dist/assets/app.js")).unwrap(),
            "x()"
        );
        // The restored file is the caller's own: writing it leaves the store alone.
        fs::write(dst.join("lib.rlib"), "changed").unwrap();
        assert_eq!(
            s.read_blob(&unhex(&got.outputs[2].blob).unwrap()).unwrap(),
            b"rlib"
        );
    }

    #[test]
    fn an_entry_needs_its_blobs_and_a_lost_blob_is_a_miss() {
        let (d, s) = store();
        let key = ActionKey::builder("t").finish();
        let mut a = Action::new("t", "", Origin::default());
        a.outputs.push(Output {
            path: "x".into(),
            kind: OutputKind::File,
            size: 1,
            blob: hex(&[9; 32]),
        });
        assert!(s.put_action(&key, &a).is_err(), "blob missing");

        let (id, _) = s.put_blob(b"x").unwrap();
        a.outputs[0].blob = hex(&id);
        s.put_action(&key, &a).unwrap();
        let p = s.blob_path(&id);
        let mut perms = fs::metadata(&p).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&p, perms).unwrap();
        fs::remove_file(&p).unwrap();
        assert!(!s.restore(&a, d.path()).unwrap());
    }

    #[test]
    fn a_torn_entry_is_a_miss() {
        let (_d, s) = store();
        let key = ActionKey::builder("t").finish();
        let p = s.entry_path(&key);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"{not json").unwrap();
        assert_eq!(s.get_action(&key).unwrap(), None);
        assert!(!p.exists());
    }

    #[test]
    fn restore_refuses_paths_outside_the_base() {
        let (d, s) = store();
        let (id, _) = s.put_blob(b"x").unwrap();
        let mut a = Action::new("t", "", Origin::default());
        a.outputs.push(Output {
            path: "../escape".into(),
            kind: OutputKind::File,
            size: 1,
            blob: hex(&id),
        });
        assert!(s.restore(&a, &d.path().join("base")).is_err());
        assert!(!d.path().join("escape").exists());
    }

    #[test]
    fn link_targets_stay_inside_the_outputs() {
        for (path, ok) in [("a", "b"), ("d/a", "../b"), ("d/e/a", "../../b")] {
            check_link(path, ok).unwrap();
        }
        for (path, bad) in [
            ("a", ".."),
            ("d/a", "../.."),
            ("a", "/etc"),
            ("a", "C:/x"),
            ("a", r"..\x"),
            ("a", ""),
            ("a", "x/../../y"),
        ] {
            assert!(check_link(path, bad).is_err(), "{path} -> {bad}");
        }

        for (path, bad) in [("m", "up/.."), ("d/m", "l/../.."), ("d/m", "../x/../y")] {
            assert!(check_link(path, bad).is_err(), "{path} -> {bad}");
        }
        check_link("m", "up/x").unwrap();
        check_link("d/m", "../l/x").unwrap();
    }

    fn with_outputs(s: &Store, outs: &[(&str, OutputKind, &[u8])]) -> Action {
        let mut a = Action::new("t", "", Origin::default());
        for (path, kind, body) in outs {
            let (id, size) = s.put_blob(body).unwrap();
            a.outputs.push(Output {
                path: (*path).into(),
                kind: *kind,
                size,
                blob: hex(&id),
            });
        }
        a
    }

    #[test]
    fn entries_cannot_write_through_links() {
        let (d, s) = store();
        let key = ActionKey::builder("t").finish();
        let escaping = with_outputs(&s, &[("out/a", OutputKind::Link, b"../../..")]);
        assert!(s.put_action(&key, &escaping).is_err());
        assert!(s.restore(&escaping, &d.path().join("w")).is_err());

        let nested = with_outputs(
            &s,
            &[
                ("out/a", OutputKind::Link, b"b"),
                ("out/a/evil", OutputKind::File, b"x"),
            ],
        );
        assert!(s.put_action(&key, &nested).is_err());
        assert!(s.restore(&nested, &d.path().join("w")).is_err());
        let twice = with_outputs(
            &s,
            &[("x", OutputKind::File, b"1"), ("x", OutputKind::File, b"2")],
        );
        assert!(s.put_action(&key, &twice).is_err());
        assert!(!d.path().join("w").exists(), "nothing was written");
    }

    #[test]
    fn restore_refuses_a_linked_directory_on_the_way() {
        let (d, s) = store();
        let w = d.path().join("w");
        let outside = d.path().join("outside");
        fs::create_dir_all(&w).unwrap();
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, w.join("dist"));
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&outside, w.join("dist"));
        if made.is_err() {
            return; // no symlink privilege on this host
        }
        let a = with_outputs(&s, &[("dist/app.js", OutputKind::File, b"x")]);
        assert!(s.restore(&a, &w).is_err());
        assert!(!outside.join("app.js").exists());
    }

    /// `n` bytes that compress well (repeated source text).
    fn text(n: usize) -> Vec<u8> {
        b"fn main() { println!(\"hello\"); }\n"
            .iter()
            .copied()
            .cycle()
            .take(n)
            .collect()
    }

    /// `n` bytes that do not compress (xorshift).
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn compress_settings_parse() {
        assert_eq!(Compress::parse("auto"), Some(Compress::Auto));
        assert_eq!(Compress::parse(" TRUE "), Some(Compress::Always));
        assert_eq!(Compress::parse("off"), Some(Compress::Never));
        assert_eq!(Compress::parse("sometimes"), None);
    }

    #[test]
    fn the_volume_probe_is_kept_and_auto_follows_it() {
        let (d, s) = store();
        let mark = fs::read_to_string(d.path().join("build").join("volume")).unwrap();
        assert_eq!(s.compresses(), mark.trim() == "copy", "{mark}");
        // The answer is read back, not probed again.
        fs::write(d.path().join("build").join("volume"), "reflink\n").unwrap();
        assert!(!Store::open(d.path().join("build")).unwrap().compresses());
        fs::write(d.path().join("build").join("volume"), "copy\n").unwrap();
        assert!(Store::open(d.path().join("build")).unwrap().compresses());
        assert!(!s.clone().with_compress(Compress::Never).compresses());
        assert!(s.with_compress(Compress::Always).compresses());
        assert!(
            fs::read_dir(d.path().join("build").join("tmp"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn a_compressible_blob_is_kept_compressed_and_reads_back_plain() {
        let (d, s) = store();
        let s = s.with_compress(Compress::Always);
        let body = text(64 * 1024);
        let (id, size) = s.put_blob(&body).unwrap();
        assert_eq!(size, body.len() as u64);
        let (path, compressed) = s.blob_file(&id).unwrap();
        assert!(compressed && path.to_string_lossy().ends_with(".zst"));
        assert!(!s.blob_path(&id).exists());
        assert!(fs::metadata(&path).unwrap().len() * 10 < size);
        assert!(s.has_blob(&id));
        assert_eq!(s.read_blob(&id).unwrap(), body);
        let mut out = Vec::new();
        s.copy_blob_to(&hex(&id), &mut out).unwrap();
        assert_eq!(out, body);
        // Storing it again keeps the one compressed file.
        assert_eq!(s.put_blob(&body).unwrap().0, id);
        let u = s.usage().unwrap();
        assert_eq!((u.blobs, u.compressed), (1, 1));
        assert!(u.bytes < size / 10);
        // A restore writes the plain bytes, with the output's mode.
        let a = with_outputs(&s, &[("bin/tool", OutputKind::Exec, &body)]);
        let w = d.path().join("w");
        assert!(s.restore(&a, &w).unwrap());
        assert_eq!(fs::read(w.join("bin/tool")).unwrap(), body);
        assert!(executable(&fs::metadata(w.join("bin/tool")).unwrap()) || cfg!(windows));
    }

    #[test]
    fn small_incompressible_or_uncompressed_blobs_stay_plain() {
        let (_d, s) = store();
        let on = s.clone().with_compress(Compress::Always);
        let small = text(COMPRESS_MIN as usize - 1);
        let (id, _) = on.put_blob(&small).unwrap();
        assert!(!on.blob_file(&id).unwrap().1, "under the minimum");
        let random = noise(64 * 1024);
        let (id, _) = on.put_blob(&random).unwrap();
        assert!(!on.blob_file(&id).unwrap().1, "saves under a tenth");
        assert_eq!(on.read_blob(&id).unwrap(), random);
        let off = s.with_compress(Compress::Never);
        let (id, _) = off.put_blob(&text(64 * 1024)).unwrap();
        assert!(!off.blob_file(&id).unwrap().1);
        let tmp: Vec<_> = fs::read_dir(off.root().join("tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "{tmp:?}");
    }

    #[test]
    fn a_plain_path_stores_a_compressed_blob_plain() {
        let (_d, s) = store();
        let s = s.with_compress(Compress::Always);
        let body = text(32 * 1024);
        let (id, _) = s.put_blob(&body).unwrap();
        let path = s.plain_blob_path(&id).unwrap().unwrap();
        assert_eq!(path, s.blob_path(&id));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(s.blob_file(&id), Some((path.clone(), false)));
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        // Asked again: the same file, nothing rewritten.
        assert_eq!(s.plain_blob_path(&id).unwrap().unwrap(), path);
        assert_eq!(s.plain_blob_path(&[9; 32]).unwrap(), None);
    }

    #[test]
    fn a_compressed_lower_store_copies_up_into_a_plain_one_and_damage_is_a_miss() {
        let d = tempfile::tempdir().unwrap();
        let lower = Store::open(d.path().join("host"))
            .unwrap()
            .with_compress(Compress::Always);
        let key = ActionKey::builder("rustc").inputs(&[3; 32]).finish();
        let body = text(48 * 1024);
        let a = with_outputs(&lower, &[("lib.rlib", OutputKind::File, &body)]);
        lower.put_action(&key, &a).unwrap();
        let id = unhex(&a.outputs[0].blob).unwrap();
        assert!(lower.blob_file(&id).unwrap().1);
        let upper = Store::open(d.path().join("build"))
            .unwrap()
            .with_compress(Compress::Never)
            .with_lower(vec![d.path().join("host")]);
        assert_eq!(upper.get_action(&key).unwrap(), Some(a.clone()));
        assert!(!upper.blob_file(&id).unwrap().1);
        assert_eq!(upper.read_blob(&id).unwrap(), body);

        // A flipped byte in a compressed lower blob: the frame fails to decode, a miss.
        let other = ActionKey::builder("rustc").inputs(&[4; 32]).finish();
        let body2 = text(40 * 1024 + 1);
        let a2 = with_outputs(&lower, &[("x.rlib", OutputKind::File, &body2)]);
        lower.put_action(&other, &a2).unwrap();
        let (zst, _) = lower
            .blob_file(&unhex(&a2.outputs[0].blob).unwrap())
            .unwrap();
        let mut bytes = fs::read(&zst).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        remove_readonly(&zst).unwrap();
        fs::write(&zst, bytes).unwrap();
        assert_eq!(upper.get_action(&other).unwrap(), None);
    }

    #[test]
    fn gc_and_clear_remove_compressed_blobs() {
        let (_d, s) = store();
        let s = s.with_compress(Compress::Always);
        let key = ActionKey::builder("t").inputs(&[5; 32]).finish();
        let a = with_outputs(&s, &[("out", OutputKind::File, &text(20_000))]);
        s.put_action(&key, &a).unwrap();
        assert_eq!(s.usage().unwrap().compressed, 1);
        let r = s.clear(None).unwrap();
        assert_eq!((r.entries, r.blobs), (1, 1));
        let u = s.usage().unwrap();
        assert_eq!((u.blobs, u.compressed), (0, 0));
    }

    #[test]
    fn pack_and_remote_bodies_carry_compressed_blobs() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("a"))
            .unwrap()
            .with_compress(Compress::Always);
        let key = ActionKey::builder("t").inputs(&[6; 32]).finish();
        let body = text(30_000);
        let a = with_outputs(&s, &[("out", OutputKind::File, &body)]);
        s.put_action(&key, &a).unwrap();
        let id = unhex(&a.outputs[0].blob).unwrap();
        // The remote body of a compressed blob is the stored frame itself.
        let z = s.blob_zstd(&id).unwrap().unwrap();
        assert_eq!(z, fs::read(s.blob_file(&id).unwrap().0).unwrap());
        assert_eq!(zstd::decode_all(&z[..]).unwrap(), body);
        let mut packed = Vec::new();
        assert!(s.pack(&key, &mut packed).unwrap());
        let t = Store::open(d.path().join("b"))
            .unwrap()
            .with_compress(Compress::Never);
        assert_eq!(t.unpack(&key, &mut &packed[..]).unwrap(), a);
        assert_eq!(t.read_blob(&id).unwrap(), body);
        assert_eq!(
            t.put_blob_zstd(&id, &z, u64::MAX).unwrap(),
            body.len() as u64
        );
    }
}
