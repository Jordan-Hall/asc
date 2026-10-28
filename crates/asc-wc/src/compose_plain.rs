//! Submodule composition for plain working copies (SPEC §10.6, §15.5).
//!
//! A gitlink path is projected as a read-only copy of the nested repository's tree at the pinned
//! commit, or as an empty read-only directory when the caller may not read the nested repository
//! (the restricted projection). This module also holds the pure configuration side shared with
//! the virtual providers: `.gitmodules` parsing and the nested repository name.
//!
//! Everything read from a tree, a `.gitmodules` file or the config is untrusted: sizes, entry
//! counts and depth are bounded, hashes are verified, and nothing is written through a symlinked
//! ancestor.
//!
//! A projection is replaced or removed only when asc can prove it made it. Every projection holds
//! a read-only [`MARKER`] file recording the submodule path, the tree, the number of entries and
//! links it was built with, and a nonce. Before a directory is replaced or deleted the marker
//! must match and a bounded walk (without following links) must find every file read-only, no
//! link beyond those recorded, and exactly the recorded entry count. Anything else, including an
//! unreadable entry or more than [`MAX_VERIFY_ENTRIES`] entries, means "not ours": the call is
//! refused and the directory is left alone.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use asc_core::codec::{CodecError, decode_envelope};
use asc_core::content::{CHUNK_THRESHOLD, ChunkAssembler, ContentError};
use asc_core::id::{FileId, TreeId};
use asc_core::object::{ChunkList, Kind, Tree};
use asc_core::path::{RepoPath, checkout_safe};
use asc_store::key::ObjKey;
use asc_store::objects::{Error as StoreError, Objects};

use crate::safe_path::{SafeRoot, case_collisions, is_link, try_fs_path};

/// Largest `.gitmodules` read.
pub const MAX_GITMODULES_BYTES: usize = 1 << 20;
/// Most submodule sections read from one `.gitmodules`.
pub const MAX_MODULES: usize = 4096;
/// Longest nested repository name segment.
const MAX_SEGMENT: usize = 100;
/// Name of the marker file inside every projection made by this module.
pub const MARKER: &str = ".asc-composed";
/// Most entries a projection may hold and the verification walk will visit; a larger tree is not
/// projected, and a larger directory is never taken for a projection.
pub const MAX_VERIFY_ENTRIES: u64 = 1_000_000;
/// Deepest directory nesting the verification walk follows.
const MAX_VERIFY_DEPTH: usize = 256;
/// Largest marker read.
const MAX_MARKER_BYTES: u64 = 4096;
const MARKER_HEAD: &str = "asc-composed 1";

/// Failures of a projection.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot decode {what}: {source}")]
    Decode {
        what: &'static str,
        source: CodecError,
    },
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("tree {0} is missing")]
    MissingTree(TreeId),
    #[error("content {0:?} is missing or does not match its id")]
    BadContent(FileId),
    #[error("{0} cannot be projected: an ancestor is a link or the name is not valid here")]
    Unsafe(RepoPath),
    #[error("{0} is not empty and is not a composed submodule")]
    NotEmpty(RepoPath),
    #[error("{path} is not a composed copy made by asc ({why}); it was left untouched")]
    NotComposed { path: RepoPath, why: String },
    #[error("the composed tree exceeds the {0} limit")]
    TooBig(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

// ------------------------------------------------------------------------------ configuration

/// One `[submodule "<name>"]` section of `.gitmodules`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub path: RepoPath,
    pub url: Option<String>,
}

type Section = (String, Option<String>, Option<String>);

/// Parses `.gitmodules` leniently: sections without a valid `path` are dropped, input beyond
/// [`MAX_GITMODULES_BYTES`] is ignored, and at most [`MAX_MODULES`] sections are returned.
pub fn parse_gitmodules(text: &str) -> Vec<Module> {
    let text = if text.len() > MAX_GITMODULES_BYTES {
        let mut end = MAX_GITMODULES_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    let mut out: Vec<Module> = Vec::new();
    let mut cur: Option<Section> = None;
    let flush = |cur: &mut Option<Section>, out: &mut Vec<Module>| {
        if let Some((name, path, url)) = cur.take()
            && let Some(path) = path.and_then(|p| RepoPath::new(p.trim_matches('/')).ok())
            && out.len() < MAX_MODULES
        {
            out.push(Module { name, path, url });
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(head) = line.strip_prefix('[') {
            flush(&mut cur, &mut out);
            let head = head.trim_end_matches(']').trim();
            cur = head
                .strip_prefix("submodule")
                .map(str::trim)
                .and_then(|n| n.strip_prefix('"')?.strip_suffix('"'))
                .map(|n| (n.to_owned(), None, None));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        if let Some((_, path, url)) = cur.as_mut() {
            match key.trim().to_ascii_lowercase().as_str() {
                "path" => *path = Some(value),
                "url" => *url = Some(value),
                _ => {}
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SEGMENT
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `name` is a nested repository name: `owner/name`, each segment plain.
pub fn valid_repo_name(name: &str) -> bool {
    let mut parts = name.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None)
        if valid_segment(a) && valid_segment(b))
}

/// The nested repository for a `.gitmodules` url, relative to `parent` (`asc/dds`):
/// `../soma.git` gives `asc/soma`; an absolute url gives its last two path segments.
pub fn derive_repo(parent: &str, url: &str) -> Option<String> {
    let url = url.trim();
    let strip = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_owned();
    let relative = url.starts_with("./") || url.starts_with("../");
    let derived = if relative {
        let mut stack: Vec<String> = parent.split('/').map(str::to_owned).collect();
        for seg in strip(url).split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    stack.pop()?;
                }
                s => stack.push(s.to_owned()),
            }
        }
        stack.join("/")
    } else {
        let path = match url.split_once("://") {
            Some((_, rest)) => rest.split_once('/')?.1,
            None => url.split_once(':')?.1, // scp-like `user@host:owner/name.git`
        };
        let path = strip(path);
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let n = segs.len();
        if n < 2 {
            return None;
        }
        format!("{}/{}", segs[n - 2], segs[n - 1])
    };
    valid_repo_name(&derived).then_some(derived)
}

/// The nested repository of each submodule path of one parent repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Submodules {
    map: BTreeMap<RepoPath, String>,
}

impl Submodules {
    /// `configured(path)` is `asc.submodule.<path>.repo`; it wins over the `.gitmodules` url.
    /// Values that are not repository names are ignored (the url default then applies).
    pub fn resolve(
        parent: &str,
        gitmodules: Option<&str>,
        paths: &[RepoPath],
        configured: impl Fn(&RepoPath) -> Option<String>,
    ) -> Self {
        let modules = gitmodules.map(parse_gitmodules).unwrap_or_default();
        let mut map = BTreeMap::new();
        for path in paths {
            let repo = configured(path)
                .filter(|r| valid_repo_name(r.trim()))
                .map(|r| r.trim().to_owned())
                .or_else(|| {
                    modules
                        .iter()
                        .find(|m| &m.path == path)
                        .and_then(|m| derive_repo(parent, m.url.as_deref()?))
                });
            if let Some(repo) = repo {
                map.insert(path.clone(), repo);
            }
        }
        Self { map }
    }

    pub fn repo_for(&self, path: &RepoPath) -> Option<&str> {
        self.map.get(path).map(String::as_str)
    }
}

/// The `asc submodule status` line of a restricted projection (SPEC §15.5).
pub fn restricted_line(path: &RepoPath, repo: &str) -> String {
    format!("{path}: restricted (no access to {repo})")
}

// ------------------------------------------------------------------------------ projection

/// Bounds on one projection.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_entries: u64,
    pub max_bytes: u64,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_bytes: 32 << 30,
            max_depth: 256,
        }
    }
}

/// What a projection wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projected {
    pub files: u64,
    pub bytes: u64,
    /// Entries not projected: names that are unsafe here or collide on a case-folding system.
    pub skipped: Vec<RepoPath>,
}

/// Whether the destination may be replaced when it holds files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replace {
    /// Only an absent path or an empty directory (the git placeholder of a gitlink).
    EmptyOnly,
    /// The path holds an earlier composed projection of this submodule.
    Composed,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_beside(dest: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!(".asc-compose-{}-{n}", std::process::id()))
}

/// Projects `tree` read-only at `at` under the gate's root, replacing what [`Replace`] allows.
/// The tree is built beside the destination and renamed into place, so a failure leaves the
/// earlier state.
pub async fn project<O: Objects>(
    objects: &O,
    gate: &SafeRoot,
    at: &RepoPath,
    tree: TreeId,
    symlinks: bool,
    replace: Replace,
    limits: &Limits,
) -> Result<Projected> {
    let dest = prepare(gate, at, replace)?;
    let tmp = temp_beside(&dest);
    fs::create_dir(&tmp).map_err(io_at(&tmp))?;
    let report = match build(objects, &tmp, at, tree, symlinks, limits).await {
        Ok(r) => r,
        Err(e) => {
            let _ = remove_tree(&tmp);
            return Err(e);
        }
    };
    swap(gate, at, &tmp, &dest, replace)?;
    Ok(report)
}

/// Projects an empty read-only directory (the restricted projection).
pub fn restrict(gate: &SafeRoot, at: &RepoPath, replace: Replace) -> Result<()> {
    let dest = prepare(gate, at, replace)?;
    let tmp = temp_beside(&dest);
    fs::create_dir(&tmp).map_err(io_at(&tmp))?;
    let sealed = write_marker(&tmp, at, RESTRICTED, 0, 0)
        .and_then(|marker| lock_file(&marker, false).map_err(io_at(&marker)))
        .and_then(|()| lock_dir(&tmp).map_err(io_at(&tmp)));
    if let Err(e) = sealed {
        let _ = remove_tree(&tmp);
        return Err(e);
    }
    swap(gate, at, &tmp, &dest, replace)
}

/// Replaces `dest` with the finished `tmp`. What is removed is checked again here, just before
/// the removal: a composed copy must still verify, and an empty-only destination is removed
/// with a call that fails when it is not empty.
fn swap(gate: &SafeRoot, at: &RepoPath, tmp: &Path, dest: &Path, replace: Replace) -> Result<()> {
    let removed = match replace {
        Replace::Composed => match fs::symlink_metadata(dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            _ => verify_existing(dest, at).and_then(|()| remove_existing(dest)),
        },
        Replace::EmptyOnly => remove_empty(dest, at),
    };
    let done = removed.and_then(|()| fs::rename(tmp, dest).map_err(io_at(dest)));
    if let Err(e) = done {
        let _ = remove_tree(tmp);
        return Err(e);
    }
    gate.forget(at);
    Ok(())
}

/// Removes a projection (or an empty placeholder) at `at`; true when something was removed.
/// A directory that is not a verified projection is refused with [`Error::NotComposed`] and
/// left alone.
pub fn clear(gate: &SafeRoot, at: &RepoPath) -> Result<bool> {
    let Some(dest) = gate.resolve(at) else {
        return Ok(false);
    };
    match fs::symlink_metadata(&dest) {
        Ok(m) if m.is_dir() && !is_link(&m) => {
            if is_empty_dir(&dest).map_err(io_at(&dest))? {
                fs::remove_dir(&dest).map_err(io_at(&dest))?;
            } else {
                verify_existing(&dest, at)?;
                remove_tree(&dest).map_err(io_at(&dest))?;
            }
            gate.forget(at);
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether the directory at `at` is a projection made by [`project`] or [`restrict`]: it carries
/// a matching marker, every file below it is still read-only, no link was added, and the entry
/// count is the one recorded (see [`check_composed`]). A directory a user can write to, such as
/// a real submodule checkout, an empty placeholder, or a projection a user has changed, is never
/// taken for one.
pub fn looks_composed(gate: &SafeRoot, at: &RepoPath) -> bool {
    check_composed(gate, at).is_ok()
}

/// [`looks_composed`] with the reason for a refusal ([`Error::NotComposed`]).
pub fn check_composed(gate: &SafeRoot, at: &RepoPath) -> Result<()> {
    let dir = gate.resolve(at).ok_or_else(|| Error::Unsafe(at.clone()))?;
    verify_existing(&dir, at)
}

fn verify_existing(dir: &Path, at: &RepoPath) -> Result<()> {
    verify_composed(dir, at, MAX_VERIFY_ENTRIES).map_err(|why| Error::NotComposed {
        path: at.clone(),
        why,
    })
}

fn is_empty_dir(dir: &Path) -> io::Result<bool> {
    Ok(fs::read_dir(dir)?.next().is_none())
}

// ------------------------------------------------------------------------------ marker

/// The `tree` field of a restricted projection.
const RESTRICTED: &str = "restricted";

/// What a projection records about itself.
#[derive(Debug, PartialEq, Eq)]
struct Marker {
    /// Debug-quoted submodule path, so a newline in a name cannot forge a field.
    path: String,
    tree: String,
    /// Entries below the directory, the marker excluded.
    entries: u64,
    /// Of those, symlinks.
    links: u64,
    nonce: String,
}

fn quote(at: &RepoPath) -> String {
    format!("{:?}", String::from_utf8_lossy(at.as_bytes()))
}

fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    (0..2)
        .map(|_| {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        })
        .collect()
}

impl Marker {
    fn render(&self) -> String {
        format!(
            "{MARKER_HEAD}\npath {}\ntree {}\nentries {}\nlinks {}\nnonce {}\n",
            self.path, self.tree, self.entries, self.links, self.nonce
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.strip_suffix('\n')?.split('\n');
        if lines.next()? != MARKER_HEAD {
            return None;
        }
        let mut field = |name: &str| lines.next()?.strip_prefix(name)?.strip_prefix(' ');
        let path = field("path")?.to_owned();
        let tree = field("tree")?.to_owned();
        let entries = field("entries")?.parse().ok()?;
        let links = field("links")?.parse().ok()?;
        let nonce = field("nonce")?.to_owned();
        let hex = nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit());
        (hex && lines.next().is_none()).then_some(Self {
            path,
            tree,
            entries,
            links,
            nonce,
        })
    }
}

/// Writes the marker into the (still unlocked) projection `dir` and returns its path; the caller
/// makes it read-only with the rest.
fn write_marker(
    dir: &Path,
    at: &RepoPath,
    tree: &str,
    entries: u64,
    links: u64,
) -> Result<PathBuf> {
    let marker = Marker {
        path: quote(at),
        tree: tree.to_owned(),
        entries,
        links,
        nonce: nonce(),
    };
    let dest = dir.join(MARKER);
    create_new(&dest, marker.render().as_bytes())?;
    Ok(dest)
}

fn read_marker(dir: &Path) -> std::result::Result<Marker, String> {
    use std::io::Read;
    let path = dir.join(MARKER);
    let meta = fs::symlink_metadata(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("it has no {MARKER} marker"),
        _ => format!("the {MARKER} marker cannot be read: {e}"),
    })?;
    if is_link(&meta) || !meta.is_file() || meta.len() > MAX_MARKER_BYTES {
        return Err(format!("the {MARKER} marker is not a plain small file"));
    }
    if !meta.permissions().readonly() {
        return Err(format!("the {MARKER} marker is writable"));
    }
    let mut text = String::new();
    fs::File::open(&path)
        .and_then(|f| f.take(MAX_MARKER_BYTES).read_to_string(&mut text))
        .map_err(|e| format!("the {MARKER} marker cannot be read: {e}"))?;
    Marker::parse(&text).ok_or_else(|| format!("the {MARKER} marker is malformed"))
}

/// Proves `dir` is the projection recorded for `at`, or says why not. The walk never follows a
/// link, visits at most `cap` entries, and treats every entry it cannot examine as a refusal.
fn verify_composed(dir: &Path, at: &RepoPath, cap: u64) -> std::result::Result<(), String> {
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if is_link(&meta) || !meta.is_dir() {
        return Err("it is not a plain directory".into());
    }
    let marker = read_marker(dir)?;
    if marker.path != quote(at) {
        return Err(format!(
            "its {MARKER} marker was made for {}, not for this path",
            marker.path
        ));
    }
    let (mut entries, mut links) = (0u64, 0u64);
    let mut stack = vec![(dir.to_owned(), 0usize)];
    while let Some((cur, depth)) = stack.pop() {
        let shown = |p: &Path| p.strip_prefix(dir).unwrap_or(p).display().to_string();
        if depth > MAX_VERIFY_DEPTH {
            return Err(format!("it nests deeper than {MAX_VERIFY_DEPTH} levels"));
        }
        let cur_meta = fs::symlink_metadata(&cur).map_err(|e| format!("{}: {e}", shown(&cur)))?;
        if !dir_is_locked(&cur_meta) {
            return Err(format!("the directory {} is writable", shown(&cur)));
        }
        let read =
            fs::read_dir(&cur).map_err(|e| format!("{} cannot be read: {e}", shown(&cur)))?;
        for entry in read {
            let entry = entry.map_err(|e| format!("{} cannot be read: {e}", shown(&cur)))?;
            let path = entry.path();
            if depth == 0 && entry.file_name() == MARKER {
                continue;
            }
            entries += 1;
            if entries > cap {
                return Err(format!("it holds more than {cap} entries"));
            }
            let m = fs::symlink_metadata(&path)
                .map_err(|e| format!("{} cannot be examined: {e}", shown(&path)))?;
            if is_link(&m) {
                links += 1;
                if links > marker.links {
                    return Err(format!("{} is a link asc did not make", shown(&path)));
                }
            } else if m.is_dir() {
                stack.push((path, depth + 1));
            } else if m.is_file() {
                if !m.permissions().readonly() {
                    return Err(format!("{} is writable", shown(&path)));
                }
            } else {
                return Err(format!("{} is not a plain file", shown(&path)));
            }
        }
    }
    if entries != marker.entries || links != marker.links {
        return Err(format!(
            "it holds {entries} entries and {links} links, asc recorded {} and {}",
            marker.entries, marker.links
        ));
    }
    Ok(())
}

/// Directories of a projection are read-only on Unix; Windows does not stop creation inside a
/// read-only directory, so the attribute is not used there.
fn dir_is_locked(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o222 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

fn prepare(gate: &SafeRoot, at: &RepoPath, replace: Replace) -> Result<PathBuf> {
    if !checkout_safe(at) {
        return Err(Error::Unsafe(at.clone()));
    }
    let dest = gate.resolve(at).ok_or_else(|| Error::Unsafe(at.clone()))?;
    // Only directories below an already verified ancestor are created.
    if let Some(up) = at.parent() {
        create_dirs(gate, &up)?;
    }
    if gate.resolve(at).is_none() {
        return Err(Error::Unsafe(at.clone()));
    }
    match fs::symlink_metadata(&dest) {
        Ok(m) if is_link(&m) || !m.is_dir() => Err(Error::NotEmpty(at.clone())),
        Ok(_) if replace == Replace::EmptyOnly => {
            if is_empty_dir(&dest).map_err(io_at(&dest))? {
                Ok(dest)
            } else {
                // A genuine projection is "not empty"; anything else is not ours at all.
                verify_existing(&dest, at)?;
                Err(Error::NotEmpty(at.clone()))
            }
        }
        Ok(_) => verify_existing(&dest, at).map(|()| dest),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(dest),
        Err(e) => Err(io_at(&dest)(e)),
    }
}

/// Creates `dir` and its missing parents one level at a time, checking each against the gate.
fn create_dirs(gate: &SafeRoot, dir: &RepoPath) -> Result<()> {
    let mut chain = vec![dir.clone()];
    while let Some(up) = chain.last().and_then(RepoPath::parent) {
        chain.push(up);
    }
    for d in chain.into_iter().rev() {
        let abs = gate.resolve(&d).ok_or_else(|| Error::Unsafe(d.clone()))?;
        match fs::symlink_metadata(&abs) {
            Ok(m) if !is_link(&m) && m.is_dir() => {}
            Ok(_) => return Err(Error::Unsafe(d)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&abs).map_err(io_at(&abs))?;
            }
            Err(e) => return Err(io_at(&abs)(e)),
        }
        gate.forget(&d);
    }
    Ok(())
}

/// Removes `dest` only if it is absent or an empty directory.
fn remove_empty(dest: &Path, at: &RepoPath) -> Result<()> {
    match fs::remove_dir(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) if fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir() && !is_link(&m)) => {
            Err(Error::NotEmpty(at.clone()))
        }
        Err(e) => Err(io_at(dest)(e)),
    }
}

/// Removes `dest` and everything below it; the caller has already verified it.
fn remove_existing(dest: &Path) -> Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(_) => remove_tree(dest).map_err(io_at(dest)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_at(dest)(e)),
    }
}

async fn load_tree<O: Objects>(objects: &O, id: TreeId) -> Result<Tree> {
    match objects.get(&ObjKey::Tree(id)).await? {
        Some(data) => {
            let tree = decode_envelope::<Tree>(&data).map_err(|source| Error::Decode {
                what: "tree",
                source,
            })?;
            if tree.id() != id {
                return Err(Error::MissingTree(id));
            }
            Ok(tree)
        }
        None => {
            let empty = Tree::new(Vec::new(), true).map_err(|_| Error::MissingTree(id))?;
            if empty.id() == id {
                Ok(empty)
            } else {
                Err(Error::MissingTree(id))
            }
        }
    }
}

async fn build<O: Objects>(
    objects: &O,
    tmp: &Path,
    at: &RepoPath,
    root: TreeId,
    symlinks: bool,
    limits: &Limits,
) -> Result<Projected> {
    let mut report = Projected::default();
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![tmp.to_owned()];
    let mut entries = 0u64;
    // Entries and links actually created: the marker records them for the verification walk.
    let (mut created, mut links) = (0u64, 0u64);
    let cap = limits.max_entries.min(MAX_VERIFY_ENTRIES);
    let mut stack: Vec<(TreeId, PathBuf, Option<RepoPath>, usize)> =
        vec![(root, tmp.to_owned(), None, 0)];
    while let Some((id, dir, rel, depth)) = stack.pop() {
        if depth > limits.max_depth {
            return Err(Error::TooBig("depth"));
        }
        let tree = load_tree(objects, id).await?;
        let joined: Vec<RepoPath> = tree
            .entries()
            .iter()
            .map(|e| match &rel {
                Some(r) => r.join(&e.name),
                None => RepoPath::from(e.name.clone()),
            })
            .collect();
        let collisions = if cfg!(any(windows, target_os = "macos")) {
            case_collisions(joined.iter())
        } else {
            Default::default()
        };
        for (e, full) in tree.entries().iter().zip(&joined) {
            entries += 1;
            if entries > cap {
                return Err(Error::TooBig("entry count"));
            }
            let local = RepoPath::from(e.name.clone());
            let is_marker = rel.is_none() && e.name.as_bytes() == MARKER.as_bytes();
            let ok = checkout_safe(full) && !collisions.contains(full) && !is_marker;
            let target = try_fs_path(&dir, &local).filter(|_| ok);
            let Some(target) = target else {
                report.skipped.push(full.clone());
                continue;
            };
            match e.kind {
                Kind::Dir => {
                    fs::create_dir(&target).map_err(io_at(&target))?;
                    created += 1;
                    dirs.push(target.clone());
                    stack.push((
                        TreeId::from_bytes(e.id),
                        target,
                        Some(full.clone()),
                        depth + 1,
                    ));
                }
                // A nested gitlink is not composed again: an empty directory, as git leaves it.
                Kind::Gitlink => {
                    fs::create_dir(&target).map_err(io_at(&target))?;
                    created += 1;
                    dirs.push(target);
                }
                Kind::File | Kind::Exec | Kind::Symlink => {
                    report.bytes = report.bytes.saturating_add(e.size);
                    if report.bytes > limits.max_bytes {
                        return Err(Error::TooBig("size"));
                    }
                    let file = FileId::from_bytes(e.id);
                    let link = write_file(objects, e.kind, file, e.size, &target, symlinks).await?;
                    created += 1;
                    if link {
                        links += 1;
                    } else {
                        files.push((target, e.kind == Kind::Exec));
                    }
                    report.files += 1;
                }
            }
        }
    }
    let marker = write_marker(tmp, at, &root.to_string(), created, links)?;
    files.push((marker, false));
    for (path, exec) in &files {
        lock_file(path, *exec).map_err(io_at(path))?;
    }
    for dir in dirs.iter().rev() {
        lock_dir(dir).map_err(io_at(dir))?;
    }
    Ok(report)
}

/// Writes one file; true when a real symlink was made.
async fn write_file<O: Objects>(
    objects: &O,
    kind: Kind,
    file: FileId,
    size: u64,
    dest: &Path,
    symlinks: bool,
) -> Result<bool> {
    if kind == Kind::Symlink || size <= CHUNK_THRESHOLD {
        let data = objects
            .get(&ObjKey::File(file))
            .await?
            .ok_or(Error::BadContent(file))?;
        if data.len() as u64 != size || FileId::of(&data) != file {
            return Err(Error::BadContent(file));
        }
        if kind == Kind::Symlink && symlinks && make_symlink(&data, dest).is_ok() {
            return Ok(true);
        }
        create_new(dest, &data)?;
        return Ok(false);
    }
    let list_bytes = objects
        .get(&ObjKey::ChunkList(file))
        .await?
        .ok_or(Error::BadContent(file))?;
    let list: ChunkList = decode_envelope(&list_bytes).map_err(|source| Error::Decode {
        what: "chunk list",
        source,
    })?;
    let chunks = list.chunks.clone();
    let mut asm = ChunkAssembler::new(list, file)?;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(io_at(dest))?;
    for (id, _) in chunks {
        let data = objects
            .get(&ObjKey::Chunk(id))
            .await?
            .ok_or(Error::BadContent(file))?;
        asm.push(&data)?;
        out.write_all(&data).map_err(io_at(dest))?;
    }
    asm.finish()?;
    Ok(false)
}

fn create_new(dest: &Path, data: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(io_at(dest))?;
    f.write_all(data).map_err(io_at(dest))
}

#[cfg(unix)]
fn lock_file(path: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }),
    )
}

#[cfg(not(unix))]
fn lock_file(path: &Path, _exec: bool) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms)
}

#[cfg(unix)]
fn lock_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o555))
}

// A read-only attribute on a Windows directory does not stop creation inside it.
#[cfg(not(unix))]
fn lock_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Removes `path` and everything below it without following links, first making read-only
/// entries writable.
fn remove_tree(path: &Path) -> io::Result<()> {
    let mut stack = vec![path.to_owned()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() && !is_link(&meta) {
            unlock(&p, &meta)?;
            for entry in fs::read_dir(&p)? {
                stack.push(entry?.path());
            }
        } else {
            unlock_file(&p, &meta);
        }
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn unlock(path: &Path, _: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(unix)]
fn unlock_file(_: &Path, _: &fs::Metadata) {}

#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute.
fn unlock(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute.
fn unlock_file(path: &Path, meta: &fs::Metadata) {
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
}

#[cfg(unix)]
fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), link)
}

#[cfg(windows)]
fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    let t = String::from_utf8_lossy(target).replace('/', "\\");
    std::os::windows::fs::symlink_file(t, link)
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_: &[u8], _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use asc_core::id::GitOid;
    use asc_core::object::Change;

    use super::*;
    use crate::plain::testutil::*;

    /// Whether file modes bind this process: root (and a process with `CAP_DAC_OVERRIDE`) writes
    /// to a read-only file, so a test that expects a refused write skips that line there.
    fn modes_bind() -> bool {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("probe");
        fs::write(&f, b"x").unwrap();
        let mut perm = fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&f, perm).unwrap();
        fs::write(&f, b"y").is_err()
    }

    #[test]
    fn gitmodules_parse_and_bound() {
        let text = "# c\n[submodule \"sdk\"]\n\tpath = packages/sdk\n\turl = ../soma.git\n[submodule \"x\"]\n\turl = nope\n[other]\npath = z\n";
        let m = parse_gitmodules(text);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, p("packages/sdk"));
        assert_eq!(m[0].url.as_deref(), Some("../soma.git"));
        let many: String = (0..MAX_MODULES + 50)
            .map(|i| format!("[submodule \"m{i}\"]\npath = d{i}\n"))
            .collect();
        assert_eq!(parse_gitmodules(&many).len(), MAX_MODULES);
        assert!(parse_gitmodules(&"a".repeat(MAX_GITMODULES_BYTES * 2)).is_empty());
    }

    #[test]
    fn repo_names_derive_from_urls() {
        assert_eq!(
            derive_repo("asc/dds", "../soma.git").as_deref(),
            Some("asc/soma")
        );
        assert_eq!(
            derive_repo("asc/dds", "../../Other/x").as_deref(),
            Some("Other/x")
        );
        assert_eq!(
            derive_repo("asc/dds", "https://forge.example/asc/soma.git").as_deref(),
            Some("asc/soma")
        );
        assert_eq!(
            derive_repo("asc/dds", "git@forge.example:asc/soma.git").as_deref(),
            Some("asc/soma")
        );
        assert_eq!(derive_repo("asc/dds", "../../../../x"), None);
        assert_eq!(derive_repo("asc/dds", "../a b.git"), None);
        assert_eq!(derive_repo("asc/dds", "https://host/solo"), None);
        assert!(!valid_repo_name("a/../b"));
        assert!(!valid_repo_name("a/b/c"));
    }

    #[test]
    fn config_wins_over_gitmodules() {
        let gm = "[submodule \"s\"]\npath = sdk\nurl = ../soma.git\n[submodule \"t\"]\npath = other\nurl = ../o.git\n";
        let paths = [p("sdk"), p("other"), p("none")];
        let subs = Submodules::resolve("asc/dds", Some(gm), &paths, |path| {
            (path == &p("sdk")).then(|| "asc/soma2".to_owned())
        });
        assert_eq!(subs.repo_for(&p("sdk")), Some("asc/soma2"));
        assert_eq!(subs.repo_for(&p("other")), Some("asc/o"));
        assert_eq!(subs.repo_for(&p("none")), None);
        let bad = Submodules::resolve("asc/dds", Some(gm), &paths, |_| Some("../../etc".into()));
        assert_eq!(bad.repo_for(&p("sdk")), Some("asc/soma"));
    }

    #[test]
    fn restricted_status_line() {
        assert_eq!(
            restricted_line(&p("packages/sdk"), "asc/soma"),
            "packages/sdk: restricted (no access to asc/soma)"
        );
    }

    #[tokio::test]
    async fn projects_read_only_and_replaces_on_a_pin_change() {
        let fx = Fixture::new();
        let v1 = fx
            .tree(&[
                ("lib.rs", Kind::File, b"v1"),
                ("d/run.sh", Kind::Exec, b"#!"),
            ])
            .await;
        let v2 = fx.tree(&[("lib.rs", Kind::File, b"v2")]).await;
        let gate = SafeRoot::new(&fx.root);
        let at = p("packages/sdk");
        let lim = Limits::default();

        let r = project(&*fx.store, &gate, &at, v1, true, Replace::EmptyOnly, &lim)
            .await
            .unwrap();
        assert_eq!((r.files, r.bytes), (2, 4));
        assert_eq!(fx.read("packages/sdk/lib.rs"), b"v1");
        assert!(
            fs::metadata(fx.root.join("packages/sdk/lib.rs"))
                .unwrap()
                .permissions()
                .readonly()
        );
        if modes_bind() {
            assert!(fs::write(fx.root.join("packages/sdk/lib.rs"), b"x").is_err());
            #[cfg(unix)]
            assert!(fs::write(fx.root.join("packages/sdk/new"), b"x").is_err());
        }

        // Occupied by files and not marked as composed: refused.
        let again = project(&*fx.store, &gate, &at, v2, true, Replace::EmptyOnly, &lim).await;
        assert!(matches!(again, Err(Error::NotEmpty(_))));
        assert_eq!(fx.read("packages/sdk/lib.rs"), b"v1");

        project(&*fx.store, &gate, &at, v2, true, Replace::Composed, &lim)
            .await
            .unwrap();
        assert_eq!(fx.read("packages/sdk/lib.rs"), b"v2");
        assert!(!fx.root.join("packages/sdk/d").exists());

        assert!(clear(&gate, &at).unwrap());
        assert!(!fx.root.join("packages/sdk").exists());
        assert!(!clear(&gate, &at).unwrap());
        assert_eq!(fs::read_dir(fx.root.join("packages")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn nested_gitlinks_stay_empty_and_limits_hold() {
        let fx = Fixture::new();
        let inner = fx.tree(&[("a", Kind::File, b"1")]).await;
        let with_link = fx
            .engine
            .apply(
                inner,
                &[(
                    p("deep/link"),
                    Change::Gitlink {
                        commit: GitOid::from_bytes([9; 20]),
                    },
                )],
            )
            .await
            .unwrap();
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("sub"),
            with_link,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        assert!(fx.root.join("sub/deep/link").is_dir());
        assert_eq!(
            fs::read_dir(fx.root.join("sub/deep/link")).unwrap().count(),
            0
        );

        let tight = Limits {
            max_entries: 1,
            ..Limits::default()
        };
        let err = project(
            &*fx.store,
            &gate,
            &p("sub2"),
            with_link,
            true,
            Replace::EmptyOnly,
            &tight,
        )
        .await;
        assert!(matches!(err, Err(Error::TooBig("entry count"))));
        assert!(!fx.root.join("sub2").exists());
        assert_eq!(
            fs::read_dir(&fx.root).unwrap().count(),
            1,
            "no temp directory is left behind"
        );
    }

    #[tokio::test]
    async fn a_missing_object_fails_without_a_projection() {
        let fx = Fixture::new();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        // The tree names a file whose bytes were never stored.
        let ghost = fx
            .engine
            .apply(
                t,
                &[(
                    p("g"),
                    Change::write(
                        Kind::File,
                        FileId::of(b"zz"),
                        2,
                        asc_core::git::blob_oid(b"zz"),
                        None,
                    )
                    .unwrap(),
                )],
            )
            .await
            .unwrap();
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        let err = project(
            &*fx.store,
            &gate,
            &p("s"),
            ghost,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await;
        assert!(matches!(err, Err(Error::BadContent(_))));
        assert!(!fx.root.join("s").exists());
    }

    #[test]
    fn restricted_projection_holds_only_its_marker() {
        let fx = Fixture::new();
        let gate = SafeRoot::new(&fx.root);
        restrict(&gate, &p("secret"), Replace::EmptyOnly).unwrap();
        assert!(fx.root.join("secret").is_dir());
        let names: Vec<_> = fs::read_dir(fx.root.join("secret"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [MARKER]);
        #[cfg(unix)]
        if modes_bind() {
            assert!(fs::write(fx.root.join("secret/x"), b"x").is_err());
        }
        restrict(&gate, &p("secret"), Replace::Composed).unwrap();
        assert!(clear(&gate, &p("secret")).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_ancestor_is_never_written_through() {
        let fx = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), fx.root.join("evil")).unwrap();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        let err = project(
            &*fx.store,
            &gate,
            &p("evil/sdk"),
            t,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await;
        assert!(matches!(err, Err(Error::Unsafe(_))));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(matches!(
            restrict(&gate, &p("evil/sdk"), Replace::EmptyOnly),
            Err(Error::Unsafe(_))
        ));
    }

    #[tokio::test]
    async fn looks_composed_tells_a_projection_from_a_user_checkout() {
        let fx = Fixture::new();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("sdk"),
            t,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        assert!(looks_composed(&gate, &p("sdk")));
        fs::create_dir_all(fx.root.join("mine")).unwrap();
        fs::write(fx.root.join("mine/file"), b"x").unwrap();
        assert!(!looks_composed(&gate, &p("mine")));
        assert!(!looks_composed(&gate, &p("absent")));
    }

    #[allow(clippy::permissions_set_readonly_false)] // tests: the user makes a file writable.
    fn make_writable(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o644);
        }
        #[cfg(not(unix))]
        perms.set_readonly(false);
        fs::set_permissions(path, perms).unwrap();
    }

    fn make_read_only(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms).unwrap();
    }

    #[tokio::test]
    async fn a_user_directory_with_a_deep_writable_file_survives() {
        let fx = Fixture::new();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        // The top level holds only a read-only file and a directory: the old check passed it.
        let mine = fx.root.join("mine");
        fs::create_dir_all(mine.join("one/two")).unwrap();
        fs::write(mine.join("ro.txt"), b"ro").unwrap();
        make_read_only(&mine.join("ro.txt"));
        fs::write(mine.join("one/two/work.txt"), b"unsaved work").unwrap();

        assert!(!looks_composed(&gate, &p("mine")));
        let err = project(
            &*fx.store,
            &gate,
            &p("mine"),
            t,
            true,
            Replace::Composed,
            &lim,
        )
        .await;
        assert!(err.is_err(), "a user directory is never replaced");
        assert!(clear(&gate, &p("mine")).is_err());
        assert_eq!(
            fs::read(mine.join("one/two/work.txt")).unwrap(),
            b"unsaved work"
        );
        assert_eq!(fs::read(mine.join("ro.txt")).unwrap(), b"ro");
    }

    #[tokio::test]
    async fn a_read_only_directory_without_the_marker_survives() {
        let fx = Fixture::new();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        let plain = fx.root.join("plain");
        fs::create_dir_all(plain.join("sub")).unwrap();
        for f in ["top.txt", "sub/deep.txt"] {
            fs::write(plain.join(f), b"x").unwrap();
            make_read_only(&plain.join(f));
        }

        assert!(!looks_composed(&gate, &p("plain")));
        let replaced = project(
            &*fx.store,
            &gate,
            &p("plain"),
            t,
            true,
            Replace::Composed,
            &lim,
        )
        .await;
        assert!(replaced.is_err());
        assert!(restrict(&gate, &p("plain"), Replace::Composed).is_err());
        assert!(clear(&gate, &p("plain")).is_err());
        assert_eq!(fs::read(plain.join("sub/deep.txt")).unwrap(), b"x");
        assert_eq!(fs::read(plain.join("top.txt")).unwrap(), b"x");
    }

    #[tokio::test]
    async fn a_genuine_composed_copy_is_replaced() {
        let fx = Fixture::new();
        let v1 = fx
            .tree(&[
                ("lib.rs", Kind::File, b"v1"),
                ("d/e/deep.rs", Kind::File, b"d1"),
            ])
            .await;
        let v2 = fx.tree(&[("lib.rs", Kind::File, b"v2")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("sdk"),
            v1,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        assert!(looks_composed(&gate, &p("sdk")));
        project(
            &*fx.store,
            &gate,
            &p("sdk"),
            v2,
            true,
            Replace::Composed,
            &lim,
        )
        .await
        .unwrap();
        assert_eq!(fx.read("sdk/lib.rs"), b"v2");
        assert!(!fx.root.join("sdk/d").exists());
        assert!(looks_composed(&gate, &p("sdk")));
        restrict(&gate, &p("sdk"), Replace::Composed).unwrap();
        assert!(looks_composed(&gate, &p("sdk")));
        assert!(clear(&gate, &p("sdk")).unwrap());
        assert!(!fx.root.join("sdk").exists());
    }

    #[tokio::test]
    async fn a_composed_copy_with_one_nested_file_made_writable_survives() {
        let fx = Fixture::new();
        let v1 = fx
            .tree(&[
                ("lib.rs", Kind::File, b"v1"),
                ("d/e/deep.rs", Kind::File, b"d1"),
            ])
            .await;
        let v2 = fx.tree(&[("lib.rs", Kind::File, b"v2")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("sdk"),
            v1,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        let deep = fx.root.join("sdk/d/e/deep.rs");
        make_writable(&deep);
        fs::write(&deep, b"my edit").unwrap();

        assert!(!looks_composed(&gate, &p("sdk")));
        let err = project(
            &*fx.store,
            &gate,
            &p("sdk"),
            v2,
            true,
            Replace::Composed,
            &lim,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a composed"), "{err}");
        assert!(err.contains("deep.rs"), "the refusal names the file: {err}");
        assert!(clear(&gate, &p("sdk")).is_err());
        assert_eq!(fs::read(&deep).unwrap(), b"my edit");
        assert_eq!(fx.read("sdk/lib.rs"), b"v1");
    }

    #[tokio::test]
    async fn a_marker_for_another_path_or_an_added_entry_is_not_ours() {
        let fx = Fixture::new();
        let t = fx
            .tree(&[("a", Kind::File, b"1"), ("d/b", Kind::File, b"2")])
            .await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("one"),
            t,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        // A copied directory carries a marker made for another path.
        copy_dir(&fx.root.join("one"), &fx.root.join("two"));
        let why = check_composed(&gate, &p("two")).unwrap_err().to_string();
        assert!(why.contains("not for this path"), "{why}");
        assert!(check_composed(&gate, &p("one")).is_ok());

        // A read-only file added by hand changes the entry count.
        let extra = fx.root.join("one/d/extra");
        let dir = fx.root.join("one/d");
        fs::write(&extra, b"mine").unwrap_or_else(|_| {
            // Unix: the directory is locked; unlock it the way the user would.
            make_dir_writable(&dir);
            fs::write(&extra, b"mine").unwrap();
        });
        make_read_only(&extra);
        let why = check_composed(&gate, &p("one")).unwrap_err().to_string();
        assert!(
            why.contains("asc recorded") || why.contains("writable"),
            "{why}"
        );
        assert!(clear(&gate, &p("one")).is_err());
        assert_eq!(fs::read(&extra).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn a_forged_or_damaged_marker_is_not_ours() {
        let fx = Fixture::new();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("sdk"),
            t,
            true,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        let marker = fx.root.join("sdk").join(MARKER);
        let good = fs::read_to_string(&marker).unwrap();
        make_writable(&marker);
        fs::write(&marker, &good).unwrap();
        // Writable marker: refused even though the text is intact.
        assert!(!looks_composed(&gate, &p("sdk")));
        let bad_entries = good.replace("entries 1", "entries 7");
        let bad_nonce = good.replace("nonce ", "nonce z");
        for bad in [
            "",
            "asc-composed 1
",
            &bad_entries,
            &bad_nonce,
        ] {
            fs::write(&marker, bad).unwrap();
            make_read_only(&marker);
            assert!(!looks_composed(&gate, &p("sdk")), "{bad:?}");
            make_writable(&marker);
        }
        fs::write(&marker, &good).unwrap();
        make_read_only(&marker);
        assert!(looks_composed(&gate, &p("sdk")));
    }

    #[test]
    fn the_walk_is_bounded_and_does_not_follow_links() {
        let fx = Fixture::new();
        let gate = SafeRoot::new(&fx.root);
        restrict(&gate, &p("sdk"), Replace::EmptyOnly).unwrap();
        let dir = gate.resolve(&p("sdk")).unwrap();
        assert!(verify_composed(&dir, &p("sdk"), 0).is_ok());
        let marker = Marker::parse(&fs::read_to_string(dir.join(MARKER)).unwrap()).unwrap();
        assert_eq!(
            (marker.entries, marker.links, marker.tree.as_str()),
            (0, 0, RESTRICTED)
        );

        // More entries than the cap: refused without finishing the walk.
        let fx2 = Fixture::new();
        let gate2 = SafeRoot::new(&fx2.root);
        let two = fx2.root.join("many");
        fs::create_dir(&two).unwrap();
        for i in 0..5 {
            fs::write(two.join(format!("f{i}")), b"x").unwrap();
        }
        let marker = Marker {
            path: quote(&p("many")),
            tree: "t".into(),
            entries: 5,
            links: 0,
            nonce: nonce(),
        };
        fs::write(two.join(MARKER), marker.render()).unwrap();
        make_read_only(&two.join(MARKER));
        for i in 0..5 {
            make_read_only(&two.join(format!("f{i}")));
        }
        // A projection's directories are locked on Unix.
        lock_dir(&two).unwrap();
        assert!(verify_composed(&two, &p("many"), 5).is_ok());
        let why = verify_composed(&two, &p("many"), 4).unwrap_err();
        assert!(why.contains("more than 4 entries"), "{why}");
        assert!(gate2.resolve(&p("many")).is_some());
        make_dir_writable(&two);
    }

    #[tokio::test]
    async fn a_link_added_to_a_composed_copy_is_not_followed_or_trusted() {
        let fx = Fixture::new();
        let t = fx.tree(&[("a", Kind::File, b"1")]).await;
        let gate = SafeRoot::new(&fx.root);
        let lim = Limits::default();
        project(
            &*fx.store,
            &gate,
            &p("sdk"),
            t,
            false,
            Replace::EmptyOnly,
            &lim,
        )
        .await
        .unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"keep").unwrap();
        let sdk = fx.root.join("sdk");
        make_dir_writable(&sdk);
        let link = sdk.join("l");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.path(), &link).is_ok();
        // A junction needs no privilege on Windows.
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(made, "the test link was not created");
        // Lock the directory again so the link, not the open directory, is the reason.
        lock_dir(&sdk).unwrap();
        let why = check_composed(&gate, &p("sdk")).unwrap_err().to_string();
        assert!(why.contains("link"), "{why}");
        assert!(clear(&gate, &p("sdk")).is_err());
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
        make_dir_writable(&sdk);
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dest = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &dest);
            } else {
                fs::copy(e.path(), &dest).unwrap();
            }
        }
    }

    #[cfg(unix)]
    fn make_dir_writable(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn make_dir_writable(_: &Path) {}
}
