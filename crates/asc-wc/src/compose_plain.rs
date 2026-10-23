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
    let strip = |s: &str| {
        s.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_owned()
    };
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
    let report = match build(objects, &tmp, tree, symlinks, limits).await {
        Ok(r) => r,
        Err(e) => {
            let _ = remove_tree(&tmp);
            return Err(e);
        }
    };
    swap(gate, at, &tmp, &dest)?;
    Ok(report)
}

/// Projects an empty read-only directory (the restricted projection).
pub fn restrict(gate: &SafeRoot, at: &RepoPath, replace: Replace) -> Result<()> {
    let dest = prepare(gate, at, replace)?;
    let tmp = temp_beside(&dest);
    fs::create_dir(&tmp).map_err(io_at(&tmp))?;
    if let Err(e) = lock_dir(&tmp) {
        let _ = remove_tree(&tmp);
        return Err(io_at(&tmp)(e));
    }
    swap(gate, at, &tmp, &dest)
}

fn swap(gate: &SafeRoot, at: &RepoPath, tmp: &Path, dest: &Path) -> Result<()> {
    let done = remove_existing(dest).and_then(|()| fs::rename(tmp, dest).map_err(io_at(dest)));
    if let Err(e) = done {
        let _ = remove_tree(tmp);
        return Err(e);
    }
    gate.forget(at);
    Ok(())
}

/// Removes a projection (or an empty placeholder) at `at`; true when something was removed.
pub fn clear(gate: &SafeRoot, at: &RepoPath) -> Result<bool> {
    let Some(dest) = gate.resolve(at) else {
        return Ok(false);
    };
    match fs::symlink_metadata(&dest) {
        Ok(m) if m.is_dir() && !is_link(&m) => {
            remove_tree(&dest).map_err(io_at(&dest))?;
            gate.forget(at);
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether the directory at `at` is a projection made by [`project`] or [`restrict`] (or an
/// empty placeholder): every entry is read-only, and on Unix so is the directory. A directory a
/// user can write to, such as a real submodule checkout, is never taken for one.
pub fn looks_composed(gate: &SafeRoot, at: &RepoPath) -> bool {
    let Some(dir) = gate.resolve(at) else {
        return false;
    };
    let Ok(meta) = fs::symlink_metadata(&dir) else {
        return false;
    };
    if !meta.is_dir() || is_link(&meta) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o222 != 0 {
            return fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_none());
        }
        true
    }
    #[cfg(not(unix))]
    {
        let Ok(entries) = fs::read_dir(&dir) else {
            return false;
        };
        for e in entries {
            let Ok(e) = e else { return false };
            let Ok(m) = fs::symlink_metadata(e.path()) else {
                return false;
            };
            if is_link(&m) || (!m.is_dir() && !m.permissions().readonly()) {
                return false;
            }
        }
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
            let empty = fs::read_dir(&dest).map_err(io_at(&dest))?.next().is_none();
            if empty {
                Ok(dest)
            } else {
                Err(Error::NotEmpty(at.clone()))
            }
        }
        Ok(_) => Ok(dest),
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
    root: TreeId,
    symlinks: bool,
    limits: &Limits,
) -> Result<Projected> {
    let mut report = Projected::default();
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![tmp.to_owned()];
    let mut entries = 0u64;
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
            if entries > limits.max_entries {
                return Err(Error::TooBig("entry count"));
            }
            let local = RepoPath::from(e.name.clone());
            let ok = checkout_safe(full) && !collisions.contains(full);
            let target = try_fs_path(&dir, &local).filter(|_| ok);
            let Some(target) = target else {
                report.skipped.push(full.clone());
                continue;
            };
            match e.kind {
                Kind::Dir => {
                    fs::create_dir(&target).map_err(io_at(&target))?;
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
                    dirs.push(target);
                }
                Kind::File | Kind::Exec | Kind::Symlink => {
                    report.bytes = report.bytes.saturating_add(e.size);
                    if report.bytes > limits.max_bytes {
                        return Err(Error::TooBig("size"));
                    }
                    let file = FileId::from_bytes(e.id);
                    let link = write_file(objects, e.kind, file, e.size, &target, symlinks).await?;
                    if !link {
                        files.push((target, e.kind == Kind::Exec));
                    }
                    report.files += 1;
                }
            }
        }
    }
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
        assert!(fs::write(fx.root.join("packages/sdk/lib.rs"), b"x").is_err());
        #[cfg(unix)]
        assert!(fs::write(fx.root.join("packages/sdk/new"), b"x").is_err());

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
        project(&*fx.store, &gate, &p("sub"), with_link, true, Replace::EmptyOnly, &lim)
            .await
            .unwrap();
        assert!(fx.root.join("sub/deep/link").is_dir());
        assert_eq!(fs::read_dir(fx.root.join("sub/deep/link")).unwrap().count(), 0);

        let tight = Limits {
            max_entries: 1,
            ..Limits::default()
        };
        let err = project(&*fx.store, &gate, &p("sub2"), with_link, true, Replace::EmptyOnly, &tight)
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
        let err = project(&*fx.store, &gate, &p("s"), ghost, true, Replace::EmptyOnly, &lim).await;
        assert!(matches!(err, Err(Error::BadContent(_))));
        assert!(!fx.root.join("s").exists());
    }

    #[test]
    fn restricted_projection_is_an_empty_read_only_directory() {
        let fx = Fixture::new();
        let gate = SafeRoot::new(&fx.root);
        restrict(&gate, &p("secret"), Replace::EmptyOnly).unwrap();
        assert!(fx.root.join("secret").is_dir());
        assert_eq!(fs::read_dir(fx.root.join("secret")).unwrap().count(), 0);
        #[cfg(unix)]
        assert!(fs::write(fx.root.join("secret/x"), b"x").is_err());
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
        let err = project(&*fx.store, &gate, &p("evil/sdk"), t, true, Replace::EmptyOnly, &lim).await;
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
        project(&*fx.store, &gate, &p("sdk"), t, true, Replace::EmptyOnly, &lim)
            .await
            .unwrap();
        assert!(looks_composed(&gate, &p("sdk")));
        fs::create_dir_all(fx.root.join("mine")).unwrap();
        fs::write(fx.root.join("mine/file"), b"x").unwrap();
        assert!(!looks_composed(&gate, &p("mine")));
        assert!(!looks_composed(&gate, &p("absent")));
    }
}
