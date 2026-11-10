//! Plain working copy (SPEC §10.4): materialise trees from the machine store and switch between
//! them, updating only changed paths and keeping the synthetic index in step.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use asc_core::codec::{CodecError, decode_envelope};
use asc_core::content::{CHUNK_THRESHOLD, ChunkAssembler, ContentError};
use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{Change, ChunkList, Entry, Kind};
use asc_core::path::{Name, RepoPath, checkout_safe};
use asc_engine::Engine;
use asc_engine::commit::Error as CommitError;
use asc_engine::tree::Error as TreeError;
use asc_git::index::{
    CacheTree, FileStat, IndexEntry, IndexError, IndexFile, PlainStat, RenderOptions, StatSource,
    render, write,
};
use asc_state::Error as StateError;
use asc_store::key::ObjKey;
use asc_store::objects::{Error as StoreError, Objects};
use asc_store::refs::Refs;
use asc_store::storeset::StoreSet;

use crate::eol::Smudge;
use crate::ignore::IgnoreSources;
use crate::redirect::Redirections;
use crate::safe_path::{Verified, case_collisions, is_link, remove_link, try_fs_path};
use crate::shared;
use crate::status::{self, EolRules, Probe};
pub use crate::switch::Mode;
use crate::switch::{Dirty, plan};

/// Failures of working-copy operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("stored content does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("content {0:?} is not in the store")]
    MissingContent(FileId),
    #[error("{} path(s) would be overwritten by the switch, first: {}", .0.len(), first(.0))]
    Conflicts(Vec<RepoPath>),
}

fn first(paths: &[RepoPath]) -> String {
    paths
        .first()
        .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .unwrap_or_default()
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// The cone of a sparse workspace (`workspaces.sparse`, one directory per line).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sparse {
    cone: Vec<RepoPath>,
}

impl Sparse {
    pub fn parse(text: &str) -> Self {
        let cone = text
            .lines()
            .filter_map(|l| RepoPath::new(l.trim().trim_matches('/')).ok())
            .collect();
        Self { cone }
    }

    /// Git cone semantics: files at the root and directly in ancestors of a cone directory, and
    /// everything inside a cone directory.
    pub fn contains(&self, path: &RepoPath) -> bool {
        let Some(parent) = path.parent() else {
            return true;
        };
        self.cone
            .iter()
            .any(|c| parent == *c || parent.starts_with_dir(c) || c.starts_with_dir(&parent))
    }
}

/// What the provider of a virtual work tree reports about a file without reading it.
pub trait Projected {
    /// Whether the file at `path` still holds the content projected from the base tree: a
    /// placeholder, or a placeholder whose data was fetched and not changed. Reading such a file
    /// would hydrate it, so status takes the base tree's entry instead.
    fn unchanged(&self, path: &RepoPath) -> io::Result<bool>;
}

/// A plain working copy: where it lives and what it reads from.
pub struct Wc<'a, O: Objects, R: Refs> {
    pub engine: &'a Engine<O, R>,
    /// The machine store; content files are cloned from it.
    pub store: &'a StoreSet,
    /// Work tree root.
    pub root: &'a Path,
    /// The synthetic index, used as asc's stat cache.
    pub index_path: &'a Path,
    /// `core.symlinks`: whether to create real symlinks (else a text file holding the target).
    pub symlinks: bool,
    /// `core.ignorecase`: tracked names also match work tree names of another case, as on a case
    /// insensitive file system.
    pub ignore_case: bool,
    pub sparse: Option<&'a Sparse>,
    pub redirections: &'a Redirections,
    pub ignore: &'a IgnoreSources,
    pub eol: &'a dyn EolRules,
    /// The provider's view of a virtual work tree; `None` for a plain one.
    pub projected: Option<&'a dyn Projected>,
    /// `asc.worktree.link = symlink`: the shared blob area that unchanged regular files are
    /// linked into (see [`crate::shared`]); `None` writes every file.
    pub shared: Option<&'a Path>,
}

/// What a switch did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwitchReport {
    pub written: u64,
    pub removed: u64,
    /// Locally modified paths left as they are (`Merge`).
    pub kept: Vec<RepoPath>,
    /// Paths that are not safe to create on this file system (`checkout_safe`).
    pub skipped: Vec<RepoPath>,
}

/// The file system path of `path` below `root` (see [`try_fs_path`]). A path that cannot be
/// represented maps to a name that never exists, so reads see "not found" and nothing is written.
pub fn fs_path(root: &Path, path: &RepoPath) -> PathBuf {
    try_fs_path(root, path).unwrap_or_else(|| root.join(".asc-unrepresentable-path"))
}

fn fs_path_checked(root: &Path, path: &RepoPath) -> Result<PathBuf> {
    try_fs_path(root, path).ok_or_else(|| Error::Io {
        path: PathBuf::from(String::from_utf8_lossy(path.as_bytes()).into_owned()),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "path cannot be represented on this file system",
        ),
    })
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_beside(dest: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!(".asc-tmp-{}-{n}", std::process::id()))
}

/// Switches the work tree from `from_root` to `to_root` and rewrites the index.
///
/// Only paths that differ are touched: files are written to a temporary file beside the target
/// and renamed into place, deletes prune emptied directories, untracked files elsewhere are left
/// alone. In `Safe` and `Merge` mode a collision returns [`Error::Conflicts`] before anything
/// is changed.
pub async fn checkout<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    from_root: TreeId,
    to_root: TreeId,
    mode: Mode,
) -> Result<SwitchReport> {
    let diff = wc.engine.leafdiff(from_root, to_root, None).await?;
    let (in_cone, outside): (Vec<_>, Vec<_>) = diff
        .into_iter()
        .partition(|(p, _)| wc.sparse.is_none_or(|s| s.contains(p)));
    let collisions = if cfg!(windows) {
        case_collisions(
            in_cone
                .iter()
                .filter(|(_, c)| !matches!(c, Change::Delete))
                .map(|(p, _)| p),
        )
    } else {
        HashSet::new()
    };
    let (safe, unsafe_paths): (Vec<_>, Vec<_>) = in_cone.into_iter().partition(|(p, c)| {
        matches!(c, Change::Delete)
            || (checkout_safe(p) && !collisions.contains(p) && try_fs_path(wc.root, p).is_some())
    });

    let mut index = IndexFile::read_or_empty(wc.index_path)?;
    let dirty = if mode == Mode::Force {
        Dirty::default()
    } else {
        collect_dirty(wc, &safe, &index)?
    };
    let plan = plan(&safe, &dirty, mode);
    if !plan.conflicts.is_empty() {
        return Err(Error::Conflicts(plan.conflicts));
    }
    let force = mode == Mode::Force;
    let mut report = SwitchReport {
        kept: plan.kept,
        skipped: unsafe_paths.into_iter().map(|(p, _)| p).collect(),
        ..SwitchReport::default()
    };
    let mut verified = Verified::default();
    for path in &plan.deletes {
        report.removed += u64::from(remove_path(wc.root, path, &mut verified)?);
    }
    // As git's checkout reads attributes from the new index, the attribute files are written
    // first and every other file is converted by the attributes of the tree being checked out.
    let (attr_files, rest): (Vec<_>, Vec<_>) =
        plan.updates.iter().partition(|(p, _)| is_attribute_file(p));
    let mut fresh: Vec<(RepoPath, Entry)> = Vec::new();
    for (path, change) in &attr_files {
        fresh.push((path.clone(), entry_of(path, change)?));
        write_path(wc, path, change, force, &mut verified).await?;
        report.written += 1;
    }
    if !attr_files.is_empty() || plan.deletes.iter().any(is_attribute_file) {
        wc.eol.reread();
    }
    for (path, change) in &rest {
        fresh.push((path.clone(), entry_of(path, change)?));
        write_path(wc, path, change, force, &mut verified).await?;
        report.written += 1;
    }
    fresh.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, change) in outside {
        if !matches!(change, Change::Delete) {
            fresh.push((path.clone(), entry_of(&path, &change)?));
        }
        index.entries.retain(|e| e.path != path);
    }
    rewrite_index(wc, &mut index, &plan.deletes, &report.skipped, fresh)?;
    Ok(report)
}

/// Whether `path` is a `.gitattributes` or `.ascattributes` file.
fn is_attribute_file(path: &RepoPath) -> bool {
    let name = path.as_bytes().rsplit(|&b| b == b'/').next().unwrap_or_default();
    name == b".gitattributes" || name == b".ascattributes"
}

fn entry_of(path: &RepoPath, change: &Change) -> Result<Entry> {
    let name: Name = path.file_name();
    Ok(match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => Entry {
            name,
            kind: *kind,
            id: *file.as_bytes(),
            size: *size,
            git: *git,
        },
        Change::Gitlink { commit } => Entry {
            name,
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: *commit,
        },
        Change::Delete => unreachable!("deletes are not entries"),
    })
}

/// Stat source for new entries: real stat inside the cone, none outside (skip-worktree).
struct ConeStat<'a> {
    inner: PlainStat,
    sparse: Option<&'a Sparse>,
}

impl StatSource for ConeStat<'_> {
    fn stat(&self, path: &RepoPath, entry: &Entry) -> FileStat {
        if self.sparse.is_some_and(|s| !s.contains(path)) {
            return FileStat {
                size: entry.size as u32,
                ..FileStat::default()
            };
        }
        self.inner.stat(path, entry)
    }
}

/// Replaces the entries of changed paths in `index` (rendered with real stat) and keeps every
/// other entry, with its stat, as it was: an untouched modified file stays stat-dirty.
fn rewrite_index<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    index: &mut IndexFile,
    deleted: &[RepoPath],
    skipped: &[RepoPath],
    fresh: Vec<(RepoPath, Entry)>,
) -> Result<()> {
    let sparse = wc.sparse;
    let outside = move |p: &RepoPath| sparse.is_some_and(|s| !s.contains(p));
    let rendered = render(
        fresh.into_iter(),
        &ConeStat {
            inner: PlainStat {
                root: wc.root.to_owned(),
            },
            sparse,
        },
        &RenderOptions {
            cache_tree: false,
            skip_worktree: Some(&outside),
            ..RenderOptions::default()
        },
    );
    let mut all: BTreeMap<RepoPath, IndexEntry> = index
        .entries
        .drain(..)
        .map(|e| (e.path.clone(), e))
        .collect();
    for p in deleted.iter().chain(skipped) {
        all.remove(p);
    }
    for e in rendered.entries {
        all.insert(e.path.clone(), e);
    }
    let entries: Vec<IndexEntry> = all.into_values().collect();
    let out = IndexFile {
        tree: Some(CacheTree::from_entries(&entries)),
        entries,
        mtime: rendered.mtime,
    };
    write(wc.index_path, &out)?;
    *index = out;
    Ok(())
}

/// Probes only the paths the switch touches for local modifications and blocking files.
fn collect_dirty<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    diff: &[(RepoPath, Change)],
    index: &IndexFile,
) -> Result<Dirty> {
    let cache: BTreeMap<&[u8], &IndexEntry> = index
        .entries
        .iter()
        .map(|e| (e.path.as_bytes(), e))
        .collect();
    let mut dirty = Dirty::default();
    let mut checked_dirs: HashSet<RepoPath> = HashSet::new();
    for (path, change) in diff {
        let tracked = cache.get(path.as_bytes()).copied();
        let state = status::probe(wc.root, wc.eol, path, tracked, index.mtime)
            .map_err(io_at(&fs_path(wc.root, path)))?;
        match (tracked, state) {
            (Some(e), Probe::File { oid, .. }) if !e.flags.skip_worktree && oid != e.oid => {
                dirty.modified.insert(path.clone());
            }
            (None, Probe::File { .. } | Probe::Other) if !matches!(change, Change::Delete) => {
                dirty.untracked.insert(path.clone());
            }
            (Some(e), Probe::Other) if !e.flags.skip_worktree => {
                dirty.modified.insert(path.clone());
            }
            _ => {}
        }
        if matches!(change, Change::Delete) {
            continue;
        }
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if !checked_dirs.insert(dir.clone()) {
                break;
            }
            if !cache.contains_key(dir.as_bytes())
                && status::ancestors_are_dirs(wc.root, &dir)
                    .map_err(io_at(&fs_path(wc.root, &dir)))?
                && fs::symlink_metadata(fs_path(wc.root, &dir)).is_ok_and(|m| !m.is_dir())
            {
                dirty.untracked.insert(dir.clone());
            }
            cur = dir.parent();
        }
    }
    Ok(dirty)
}

/// Removes `path` and the directories it leaves empty (never the root). Returns whether a file
/// was removed. Nothing is removed through a symlinked ancestor.
fn remove_path(root: &Path, path: &RepoPath, verified: &mut Verified) -> Result<bool> {
    let Some(abs) = try_fs_path(root, path) else {
        return Ok(false);
    };
    if !verified.check_parent(root, path).map_err(io_at(&abs))? {
        return Ok(false);
    }
    let removed = match fs::symlink_metadata(&abs) {
        Ok(m) if m.is_dir() && !is_link(&m) => {
            let gone = fs::remove_dir(&abs).is_ok();
            if gone {
                verified.forget(path);
            }
            gone
        }
        Ok(_) => {
            remove_link(&abs).map_err(io_at(&abs))?;
            true
        }
        Err(_) => false,
    };
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if fs::remove_dir(fs_path(root, &dir)).is_err() {
            break;
        }
        verified.forget(&dir);
        cur = dir.parent();
    }
    Ok(removed)
}

async fn write_path<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    path: &RepoPath,
    change: &Change,
    force: bool,
    verified: &mut Verified,
) -> Result<()> {
    let dest = fs_path_checked(wc.root, path)?;
    verified
        .ensure_parent(wc.root, path, force)
        .map_err(io_at(&dest))?;
    // A symlink at the destination is replaced, never followed.
    if fs::symlink_metadata(&dest).is_ok_and(|m| is_link(&m)) {
        remove_link(&dest).map_err(io_at(&dest))?;
    }
    if clear_dir_in_the_way(&dest, force)? {
        verified.forget(path);
    }
    match change {
        Change::Gitlink { .. } => {
            if fs::symlink_metadata(&dest).is_ok_and(|m| !m.is_dir()) {
                remove_link(&dest).map_err(io_at(&dest))?;
            }
            fs::create_dir_all(&dest).map_err(io_at(&dest))
        }
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => {
            let tmp = temp_beside(&dest);
            let smudge = if *kind == Kind::Symlink {
                Smudge::None
            } else {
                wc.eol.checkout(path)
            };
            let made = match wc.shared {
                // A converted file differs from its blob, so it is never a shared link.
                _ if smudge != Smudge::None => {
                    materialise_smudged(wc, *kind, *file, *size, smudge, &tmp).await
                }
                Some(area) if *kind != Kind::Symlink => {
                    match link_shared(wc, area, *kind, *file, *size, *git, &tmp).await {
                        Ok(true) => Ok(()),
                        Ok(false) => materialise(wc, *kind, *file, *size, &tmp).await,
                        Err(e) => Err(e),
                    }
                }
                _ => materialise(wc, *kind, *file, *size, &tmp).await,
            };
            if let Err(e) = made {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            fs::rename(&tmp, &dest).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                io_at(&dest)(e)
            })
        }
        Change::Delete => Ok(()),
    }
}

/// A directory where a file goes: removed when empty, or entirely under `force`. Returns whether
/// one was there.
fn clear_dir_in_the_way(dest: &Path, force: bool) -> Result<bool> {
    if fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
        let r = if force {
            fs::remove_dir_all(dest)
        } else {
            fs::remove_dir(dest)
        };
        r.map_err(io_at(dest))?;
        return Ok(true);
    }
    Ok(false)
}

/// Writes the content of `file` to `tmp` as a regular file, a symlink or (without symlink
/// support) a text file holding the link target.
async fn materialise<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    kind: Kind,
    file: FileId,
    size: u64,
    tmp: &Path,
) -> Result<()> {
    if kind == Kind::Symlink {
        let target = wc
            .store
            .get(&ObjKey::File(file))
            .await?
            .ok_or(Error::MissingContent(file))?;
        // A committed link into a shared area would read as that area's file: never made.
        if wc.symlinks && !shared::names_area_file(&target) && make_symlink(&target, tmp).is_ok() {
            return Ok(());
        }
        return fs::write(tmp, &target).map_err(io_at(tmp));
    }
    if size <= CHUNK_THRESHOLD {
        let local = wc
            .store
            .store
            .content_path(file)
            .or_else(|| wc.store.cache.content_path(file));
        match local {
            // Never a hardlink: an editor must not be able to corrupt the shared store.
            Some(src) => {
                reflink_copy::reflink_or_copy(&src, tmp).map_err(io_at(tmp))?;
            }
            None => {
                let data = wc
                    .store
                    .get(&ObjKey::File(file))
                    .await?
                    .ok_or(Error::MissingContent(file))?;
                fs::write(tmp, &data).map_err(io_at(tmp))?;
            }
        }
    } else {
        write_chunked(wc.store, file, tmp).await?;
    }
    set_mode(tmp, kind == Kind::Exec).map_err(io_at(tmp))
}

/// Writes `file` to `tmp` as a regular file with the line endings a checkout under `smudge`
/// gives (git's `crlf_to_worktree`). The file is read in pieces, once to count its line endings
/// and once more to convert it into a sibling file, so a large blob is never held in memory.
async fn materialise_smudged<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    kind: Kind,
    file: FileId,
    size: u64,
    smudge: Smudge,
    tmp: &Path,
) -> Result<()> {
    materialise(wc, kind, file, size, tmp).await?;
    let mut stats = crate::eol::Stats::default();
    for_each_piece(tmp, |piece| {
        stats.feed(piece);
        Ok(())
    })?;
    if !stats.finish().smudges(smudge) {
        return Ok(());
    }
    // A fresh name, opened only if nothing is there yet, so no existing entry or link is
    // followed.
    let converted = temp_beside(tmp);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&converted)
        .map_err(io_at(&converted))?;
    let mut out = io::BufWriter::new(file);
    let mut conv = crate::eol::LfToCrlf::default();
    let mut buf = Vec::new();
    let written = for_each_piece(tmp, |piece| {
        buf.clear();
        conv.convert(piece, &mut buf);
        out.write_all(&buf)
    })
    .and_then(|()| out.flush().map_err(io_at(&converted)));
    drop(out);
    let written =
        written.and_then(|()| set_mode(&converted, kind == Kind::Exec).map_err(io_at(&converted)));
    if let Err(e) = written {
        let _ = fs::remove_file(&converted);
        return Err(e);
    }
    fs::rename(&converted, tmp).map_err(io_at(tmp))
}

/// Calls `each` with the content of the file at `path`, one buffer-sized piece at a time.
fn for_each_piece(path: &Path, mut each: impl FnMut(&[u8]) -> io::Result<()>) -> Result<()> {
    let mut file = fs::File::open(path).map_err(io_at(path))?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = match io::Read::read(&mut file, &mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_at(path)(e)),
        };
        each(&buf[..n]).map_err(io_at(path))?;
    }
}

/// Links `tmp` to the sealed area file of blob `git`, writing and sealing that file first when
/// the area lacks it. `false` when no link can be made here (no symlink rights, or an area file
/// someone wrote through): the caller then writes the file itself.
async fn link_shared<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    area: &Path,
    kind: Kind,
    file: FileId,
    size: u64,
    git: GitOid,
    tmp: &Path,
) -> Result<bool> {
    let exec = kind == Kind::Exec;
    let target = shared::path_in(area, git, exec);
    match fs::metadata(&target) {
        Ok(m) if shared::is_sealed(&m) => {}
        // Changed through a link: it holds someone's edit, so it is neither reused nor replaced.
        Ok(_) => return Ok(false),
        Err(_) => {
            let fan = target
                .parent()
                .ok_or_else(|| io_at(&target)(io::ErrorKind::InvalidInput.into()))?;
            fs::create_dir_all(fan).map_err(io_at(fan))?;
            let part = temp_beside(&target);
            let sealed = async {
                materialise(wc, kind, file, size, &part).await?;
                shared::seal(&part, exec).map_err(io_at(&part))?;
                // Another checkout may have sealed the same blob meanwhile: either copy will do.
                match fs::rename(&part, &target) {
                    Err(_) if fs::metadata(&target).is_ok_and(|m| shared::is_sealed(&m)) => Ok(()),
                    r => r.map_err(io_at(&target)),
                }
            }
            .await;
            if sealed.is_err() {
                remove_sealed(&part);
            }
            sealed?;
        }
    }
    Ok(shared::make_link(&target, tmp).is_ok())
}

/// Removes a part-written or sealed (read-only) file, ignoring failure.
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute.
fn remove_sealed(path: &Path) {
    if let Ok(m) = fs::metadata(path) {
        let mut perms = m.permissions();
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
    let _ = fs::remove_file(path);
}

/// Streams the verified chunks of a chunked file into `tmp`.
async fn write_chunked(store: &StoreSet, file: FileId, tmp: &Path) -> Result<()> {
    let list_bytes = store
        .get(&ObjKey::ChunkList(file))
        .await?
        .ok_or(Error::MissingContent(file))?;
    let list: ChunkList = decode_envelope(&list_bytes)?;
    let chunks = list.chunks.clone();
    let mut asm = ChunkAssembler::new(list, file)?;
    let mut out = fs::File::create(tmp).map_err(io_at(tmp))?;
    for (id, _) in chunks {
        let data = store
            .get(&ObjKey::Chunk(id))
            .await?
            .ok_or(Error::MissingContent(file))?;
        asm.push(&data)?;
        out.write_all(&data).map_err(io_at(tmp))?;
    }
    asm.finish()?;
    Ok(())
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
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute.
fn set_mode(path: &Path, _exec: bool) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
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
pub(crate) mod testutil {
    use std::sync::Arc;

    use asc_core::codec::Enveloped;
    use asc_core::content::{Body, ingest};
    use asc_core::git::blob_oid;
    use asc_core::path::RefName;
    use asc_store::cachedir::CacheDir;
    use asc_store::codec::encode_stored;
    use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, RepoKey};
    use async_trait::async_trait;

    use super::*;
    use crate::status::NoEol;

    pub struct NoRefs;

    #[async_trait]
    impl Refs for NoRefs {
        async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
            Ok(None)
        }
        async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
            Ok(Vec::new())
        }
        async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
            Err(RefError::Corrupt("unused".into()))
        }
        async fn log(
            &self,
            _: RepoKey,
            _: &RefName,
            _: u32,
        ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
            Ok(Vec::new())
        }
    }

    pub fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// A store, an engine and a work tree root in a temp directory.
    pub struct Fixture {
        pub _dir: tempfile::TempDir,
        pub store: Arc<StoreSet>,
        pub engine: Engine<StoreSet, NoRefs>,
        pub root: PathBuf,
        pub index: PathBuf,
        pub redirections: Redirections,
        pub ignore: IgnoreSources,
        pub sparse: Option<Sparse>,
        pub symlinks: bool,
        pub shared: Option<PathBuf>,
    }

    impl Fixture {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path();
            let open = |n: &str| CacheDir::open(base.join(n), base.join("tmp")).unwrap();
            let store = Arc::new(StoreSet {
                store: open("store"),
                cache: open("cache"),
            });
            let root = base.join("work");
            fs::create_dir_all(&root).unwrap();
            let ignore = IgnoreSources {
                asc_excludes: base.join("none-asc"),
                core_excludes: None,
                local_exclude: root.join(".asc/info/exclude"),
                common_exclude: base.join("common-exclude"),
            };
            Self {
                engine: Engine::new(store.clone(), Arc::new(NoRefs), RepoKey(1)),
                store,
                index: base.join("index"),
                root,
                redirections: Redirections::default(),
                ignore,
                sparse: None,
                symlinks: true,
                shared: None,
                _dir: dir,
            }
        }

        pub fn wc(&self) -> Wc<'_, StoreSet, NoRefs> {
            Wc {
                engine: &self.engine,
                store: &self.store,
                root: &self.root,
                index_path: &self.index,
                symlinks: self.symlinks,
                ignore_case: false,
                sparse: self.sparse.as_ref(),
                redirections: &self.redirections,
                ignore: &self.ignore,
                eol: &NoEol,
                projected: None,
                shared: self.shared.as_deref(),
            }
        }

        /// Stores the files and returns the tree holding them.
        pub async fn tree(&self, files: &[(&str, Kind, &[u8])]) -> TreeId {
            let mut changes = Vec::new();
            for (name, kind, data) in files {
                let ing = ingest(io::Cursor::new(data.to_vec())).unwrap();
                match ing.body {
                    Body::Inline(b) => {
                        self.store
                            .put(&ObjKey::File(ing.file), encode_stored(&b))
                            .await
                            .unwrap();
                    }
                    Body::Chunked { list, chunks } => {
                        let env = list.to_envelope();
                        self.store
                            .put(&ObjKey::ChunkList(ing.file), encode_stored(&env))
                            .await
                            .unwrap();
                        for (id, c) in chunks {
                            self.store
                                .put(&ObjKey::Chunk(id), encode_stored(&c))
                                .await
                                .unwrap();
                        }
                    }
                }
                assert_eq!(ing.git, blob_oid(data));
                changes.push((
                    p(name),
                    Change::write(*kind, ing.file, ing.size, ing.git, None).unwrap(),
                ));
            }
            self.engine
                .apply(Engine::<StoreSet, NoRefs>::empty_tree_id(), &changes)
                .await
                .unwrap()
        }

        pub fn empty(&self) -> TreeId {
            Engine::<StoreSet, NoRefs>::empty_tree_id()
        }

        pub fn read(&self, rel: &str) -> Vec<u8> {
            fs::read(self.root.join(rel)).unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use asc_core::git::blob_oid;

    use super::testutil::*;
    use super::*;

    const F: Kind = Kind::File;

    fn t1() -> Vec<(&'static str, Kind, &'static [u8])> {
        vec![
            ("a.txt", F, b"one\n"),
            ("d/b.txt", F, b"two\n"),
            ("e/f/g.txt", F, b"three\n"),
            ("run.sh", Kind::Exec, b"#!/bin/sh\n"),
        ]
    }

    fn t2() -> Vec<(&'static str, Kind, &'static [u8])> {
        vec![
            ("a.txt", F, b"one changed\n"),
            ("e/f/g.txt", F, b"three\n"),
            ("n.txt", F, b"new\n"),
            ("run.sh", Kind::Exec, b"#!/bin/sh\n"),
        ]
    }

    #[tokio::test]
    async fn materialises_files_and_writes_the_index() {
        let fx = Fixture::new();
        let to = fx.tree(&t1()).await;
        let rep = checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        assert_eq!(rep.written, 4);
        assert_eq!(fx.read("d/b.txt"), b"two\n");
        assert_eq!(fx.read("e/f/g.txt"), b"three\n");
        let idx = IndexFile::read(&fx.index).unwrap();
        let paths: Vec<_> = idx.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            [p("a.txt"), p("d/b.txt"), p("e/f/g.txt"), p("run.sh")]
        );
        assert_eq!(idx.entries[0].oid, blob_oid(b"one\n"));
        assert!(idx.entries.iter().all(|e| e.size > 0 && e.mtime.secs > 0));
        assert!(idx.tree.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = fs::metadata(fx.root.join("run.sh")).unwrap();
            assert_ne!(m.permissions().mode() & 0o111, 0);
        }
    }

    /// Every file is checked out with CRLF line endings.
    struct AllCrlf;

    impl EolRules for AllCrlf {
        fn eol(&self, _: &RepoPath) -> status::Eol {
            status::Eol::Keep
        }

        fn checkout(&self, _: &RepoPath) -> Smudge {
            Smudge::Crlf
        }
    }

    #[tokio::test]
    async fn a_converted_checkout_streams_keeps_the_mode_and_leaves_no_temporary_file() {
        let fx = Fixture::new();
        // A CRLF straddles the 64 KiB read size, so the pair spans two pieces.
        let mut big = vec![b'x'; (1 << 16) - 1];
        big.extend_from_slice(b"\r\nnext\nlast\n");
        let script: &[u8] = b"#!/bin/sh\necho\n";
        let to = fx
            .tree(&[("big.txt", F, &big), ("run.sh", Kind::Exec, script)])
            .await;
        let mut wc = fx.wc();
        wc.eol = &AllCrlf;
        checkout(&wc, fx.empty(), to, Mode::Safe).await.unwrap();
        let mut want = vec![b'x'; (1 << 16) - 1];
        want.extend_from_slice(b"\r\nnext\r\nlast\r\n");
        assert_eq!(fx.read("big.txt"), want);
        assert_eq!(fx.read("run.sh"), b"#!/bin/sh\r\necho\r\n");
        let names: Vec<String> = fs::read_dir(&fx.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".asc-tmp-"))
            .collect();
        assert!(names.is_empty(), "{names:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = fs::metadata(fx.root.join("run.sh")).unwrap();
            assert_ne!(m.permissions().mode() & 0o111, 0);
        }
    }

    #[tokio::test]
    async fn a_committed_link_into_a_shared_area_is_never_made() {
        let fx = Fixture::new();
        let areas = fx._dir.path().join(shared::AREA);
        shared::trust_area(&areas);
        let area = areas.join(asc_state::RepoId::new().to_string());
        let target = shared::path_in(&area, blob_oid(b"secret"), false);
        let text = target.to_string_lossy().into_owned();
        let to = fx.tree(&[("evil", Kind::Symlink, text.as_bytes())]).await;
        checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        let meta = fs::symlink_metadata(fx.root.join("evil")).unwrap();
        assert!(!is_link(&meta));
        assert_eq!(fx.read("evil"), text.as_bytes());
    }

    #[tokio::test]
    async fn linked_executables_have_their_own_shared_copy() {
        let mut fx = Fixture::new();
        let areas = fx._dir.path().join(shared::AREA);
        shared::trust_area(&areas);
        let area = areas.join(asc_state::RepoId::new().to_string());
        fx.shared = Some(area.clone());
        let to = fx.tree(&t1()).await;
        checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        let run = fx.root.join("run.sh");
        let Some(blob) = shared::link_blob(&run) else {
            // No symlink rights here: link mode wrote plain files instead.
            assert_eq!(fx.read("run.sh"), b"#!/bin/sh\n");
            return;
        };
        assert_eq!(blob, (blob_oid(b"#!/bin/sh\n"), true));
        assert_eq!(
            fs::read_link(&run).unwrap(),
            shared::path_in(&area, blob.0, true)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&run).unwrap().permissions().mode() & 0o777,
                0o555
            );
        }
        let idx = IndexFile::read(&fx.index).unwrap();
        let e = idx.entries.iter().find(|e| e.path == p("run.sh")).unwrap();
        // The stat cache holds the shared file's stat, as status reads it.
        assert_eq!(u64::from(e.size), fs::metadata(&run).unwrap().len());
    }

    #[tokio::test]
    async fn switch_touches_only_changed_paths_and_keeps_untracked() {
        let fx = Fixture::new();
        let (a, b) = (fx.tree(&t1()).await, fx.tree(&t2()).await);
        checkout(&fx.wc(), fx.empty(), a, Mode::Safe).await.unwrap();
        fs::write(fx.root.join("untracked.txt"), b"mine").unwrap();
        let before = fs::metadata(fx.root.join("e/f/g.txt"))
            .unwrap()
            .modified()
            .unwrap();
        let rep = checkout(&fx.wc(), a, b, Mode::Safe).await.unwrap();
        assert_eq!((rep.written, rep.removed), (2, 1));
        assert_eq!(fx.read("a.txt"), b"one changed\n");
        assert!(!fx.root.join("d").exists(), "emptied directory is pruned");
        assert_eq!(fx.read("untracked.txt"), b"mine");
        let after = fs::metadata(fx.root.join("e/f/g.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
        let idx = IndexFile::read(&fx.index).unwrap();
        assert_eq!(idx.entries.len(), 4);
        assert!(idx.entries.iter().any(|e| e.path == p("n.txt")));
    }

    #[tokio::test]
    async fn safe_refuses_a_dirty_conflict_merge_keeps_force_overwrites() {
        let fx = Fixture::new();
        let (a, b) = (fx.tree(&t1()).await, fx.tree(&t2()).await);
        checkout(&fx.wc(), fx.empty(), a, Mode::Safe).await.unwrap();
        fs::write(fx.root.join("a.txt"), b"local edit that is longer\n").unwrap();

        let err = checkout(&fx.wc(), a, b, Mode::Safe).await.unwrap_err();
        assert!(
            matches!(&err, Error::Conflicts(c) if c == &[p("a.txt")]),
            "{err}"
        );
        assert_eq!(fx.read("a.txt"), b"local edit that is longer\n");
        assert!(fx.root.join("d/b.txt").exists(), "nothing was applied");

        let rep = checkout(&fx.wc(), a, b, Mode::Merge).await.unwrap();
        assert_eq!(rep.kept, [p("a.txt")]);
        assert_eq!(fx.read("a.txt"), b"local edit that is longer\n");
        assert!(!fx.root.join("d/b.txt").exists());

        let rep = checkout(&fx.wc(), a, b, Mode::Force).await.unwrap();
        assert!(rep.kept.is_empty());
        assert_eq!(fx.read("a.txt"), b"one changed\n");
    }

    #[tokio::test]
    async fn untracked_file_in_the_way_conflicts_in_safe_mode() {
        let fx = Fixture::new();
        let (a, b) = (fx.tree(&t1()).await, fx.tree(&t2()).await);
        checkout(&fx.wc(), fx.empty(), a, Mode::Safe).await.unwrap();
        fs::write(fx.root.join("n.txt"), b"untracked").unwrap();
        let err = checkout(&fx.wc(), a, b, Mode::Safe).await.unwrap_err();
        assert!(
            matches!(&err, Error::Conflicts(c) if c == &[p("n.txt")]),
            "{err}"
        );
        checkout(&fx.wc(), a, b, Mode::Force).await.unwrap();
        assert_eq!(fx.read("n.txt"), b"new\n");
    }

    #[tokio::test]
    async fn chunked_files_stream_back_verified() {
        let fx = Fixture::new();
        let big: Vec<u8> = (0..5 * 1024 * 1024u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let to = fx.tree(&[("big.bin", F, &big)]).await;
        checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        assert!(fx.read("big.bin") == big);
    }

    #[tokio::test]
    async fn symlinks_become_text_files_when_unsupported() {
        let mut fx = Fixture::new();
        fx.symlinks = false;
        let to = fx
            .tree(&[("l", Kind::Symlink, b"a.txt"), ("a.txt", F, b"x")])
            .await;
        checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        assert_eq!(fx.read("l"), b"a.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_created_when_supported() {
        let fx = Fixture::new();
        let to = fx
            .tree(&[("l", Kind::Symlink, b"a.txt"), ("a.txt", F, b"x")])
            .await;
        checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        assert_eq!(
            fs::read_link(fx.root.join("l")).unwrap(),
            Path::new("a.txt")
        );
    }

    #[tokio::test]
    async fn sparse_cone_skips_outside_paths_with_skip_worktree() {
        let mut fx = Fixture::new();
        fx.sparse = Some(Sparse::parse("d\n"));
        let to = fx.tree(&t1()).await;
        let rep = checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        assert_eq!(rep.written, 3);
        assert!(fx.root.join("a.txt").exists() && fx.root.join("d/b.txt").exists());
        assert!(!fx.root.join("e").exists());
        let idx = IndexFile::read(&fx.index).unwrap();
        let g = idx
            .entries
            .iter()
            .find(|e| e.path == p("e/f/g.txt"))
            .unwrap();
        assert!(g.flags.skip_worktree);
        assert!(!idx.entries[0].flags.skip_worktree);
    }

    #[test]
    fn cone_membership() {
        let s = Sparse::parse("a/b\n");
        for (path, want) in [
            ("top.txt", true),
            ("a/x.txt", true),
            ("a/b/c/d.txt", true),
            ("a/z/q.txt", false),
            ("other/x.txt", false),
        ] {
            assert_eq!(s.contains(&p(path)), want, "{path}");
        }
    }

    /// A directory outside the work tree holding `keep`, and the tree root's symlink to it at `at`.
    fn outside_link(fx: &Fixture, at: &str) -> Option<PathBuf> {
        let outside = fx.root.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), b"keep").unwrap();
        crate::safe_path::symlink_dir(&outside, &fx.root.join(at)).then_some(outside)
    }

    fn outside_untouched(outside: &Path) {
        let names: Vec<_> = fs::read_dir(outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("keep")]);
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn force_checkout_never_writes_through_a_symlinked_directory() {
        let fx = Fixture::new();
        let Some(outside) = outside_link(&fx, "a") else {
            return;
        };
        let to = fx
            .tree(&[("a/x", F, b"payload"), ("a/y/z", F, b"deep")])
            .await;
        checkout(&fx.wc(), fx.empty(), to, Mode::Force)
            .await
            .unwrap();
        outside_untouched(&outside);
        assert_eq!(fx.read("a/x"), b"payload");
        assert_eq!(fx.read("a/y/z"), b"deep");
        let meta = fs::symlink_metadata(fx.root.join("a")).unwrap();
        assert!(meta.is_dir() && !is_link(&meta));
    }

    #[tokio::test]
    async fn safe_checkout_over_a_symlinked_directory_conflicts_untouched() {
        let fx = Fixture::new();
        let Some(outside) = outside_link(&fx, "a") else {
            return;
        };
        let to = fx.tree(&[("a/x", F, b"payload")]).await;
        let r = checkout(&fx.wc(), fx.empty(), to, Mode::Safe).await;
        assert!(r.is_err());
        outside_untouched(&outside);
    }

    #[tokio::test]
    async fn safe_switch_does_not_probe_through_a_symlink_replaced_by_a_directory() {
        let fx = Fixture::new();
        let outside = fx.root.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("zero"), b"keep").unwrap();
        let t1 = fx.tree(&[("a", Kind::Symlink, b"../outside")]).await;
        checkout(&fx.wc(), fx.empty(), t1, Mode::Safe)
            .await
            .unwrap();
        if !fs::symlink_metadata(fx.root.join("a")).is_ok_and(|m| is_link(&m)) {
            return;
        }
        // `outside/zero` exists, so a probe following the link reports a collision.
        let t2 = fx.tree(&[("a/zero", F, b"new")]).await;
        checkout(&fx.wc(), t1, t2, Mode::Safe).await.unwrap();
        assert_eq!(fs::read(outside.join("zero")).unwrap(), b"keep");
        assert_eq!(fx.read("a/zero"), b"new");
    }

    #[test]
    fn probe_under_a_symlinked_ancestor_is_missing() {
        let fx = Fixture::new();
        let Some(_) = outside_link(&fx, "a") else {
            return;
        };
        fs::write(fx.root.parent().unwrap().join("outside/zero"), b"x").unwrap();
        let got = status::probe(&fx.root, &crate::status::NoEol, &p("a/zero"), None, None).unwrap();
        assert_eq!(got, Probe::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn probe_never_reads_a_non_regular_file() {
        let fx = Fixture::new();
        let fifo = fx.root.join("pipe");
        if !std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
        let got = status::probe(&fx.root, &crate::status::NoEol, &p("pipe"), None, None).unwrap();
        assert_eq!(got, Probe::Other);
    }

    #[tokio::test]
    async fn checkout_refuses_asc_info_paths() {
        let fx = Fixture::new();
        let to = fx
            .tree(&[
                (
                    ".asc/info/exclude",
                    F,
                    b"*
",
                ),
                (
                    ".ASC/INFO/exclude",
                    F,
                    b"*
",
                ),
                ("ok", F, b"1"),
            ])
            .await;
        let r = checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        let mut skipped: Vec<_> = r.skipped.iter().map(|p| p.to_string()).collect();
        skipped.sort();
        assert_eq!(skipped, [".ASC/INFO/exclude", ".asc/info/exclude"]);
        assert!(!fx.root.join(".asc/info/exclude").exists());
        assert_eq!(fx.read("ok"), b"1");
    }

    /// A tree whose `.asc` is a symlink to `x` (holding `x/info/exclude` and `x/redirections`)
    /// would redirect the local exclude and redirection reads; the `.asc` entry is skipped.
    #[tokio::test]
    async fn checkout_refuses_a_asc_symlink() {
        let fx = Fixture::new();
        let to = fx
            .tree(&[
                (".asc", Kind::Symlink, b"x"),
                (".ASC", Kind::Symlink, b"x"),
                ("x/info/exclude", F, b"*\n"),
                ("x/redirections", F, b"src\n"),
            ])
            .await;
        let r = checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        let mut skipped: Vec<_> = r.skipped.iter().map(|p| p.to_string()).collect();
        skipped.sort();
        assert_eq!(skipped, [".ASC", ".asc"]);
        assert!(fs::symlink_metadata(fx.root.join(".asc")).is_err());
        assert!(
            crate::redirect::Redirections::load(&fx.root)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fx.ignore.local(&fx.root), fx.ignore.common_exclude);
    }

    #[tokio::test]
    async fn deletes_do_not_go_through_a_symlinked_parent() {
        let fx = Fixture::new();
        let t1 = fx.tree(&[("a/keep", F, b"mine")]).await;
        checkout(&fx.wc(), fx.empty(), t1, Mode::Safe)
            .await
            .unwrap();
        fs::remove_dir_all(fx.root.join("a")).unwrap();
        let Some(outside) = outside_link(&fx, "a") else {
            return;
        };
        let r = checkout(&fx.wc(), t1, fx.empty(), Mode::Force)
            .await
            .unwrap();
        assert_eq!(r.removed, 0);
        outside_untouched(&outside);
    }

    #[tokio::test]
    async fn a_symlink_replaced_by_a_directory_is_removed_not_followed() {
        let fx = Fixture::new();
        let outside = fx.root.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), b"keep").unwrap();
        let t1 = fx.tree(&[("d", Kind::Symlink, b"../outside")]).await;
        checkout(&fx.wc(), fx.empty(), t1, Mode::Safe)
            .await
            .unwrap();
        if !fs::symlink_metadata(fx.root.join("d")).is_ok_and(|m| is_link(&m)) {
            return;
        }
        let t2 = fx.tree(&[("d/f", F, b"new")]).await;
        checkout(&fx.wc(), t1, t2, Mode::Safe).await.unwrap();
        outside_untouched(&outside);
        assert_eq!(fx.read("d/f"), b"new");
        assert!(!is_link(&fs::symlink_metadata(fx.root.join("d")).unwrap()));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_skips_unrepresentable_and_colliding_paths() {
        let fx = Fixture::new();
        let to = fx
            .tree(&[
                ("ok", F, b"1"),
                ("aux", F, b"2"),
                ("dir/nul.txt", F, b"3"),
                ("dot.", F, b"4"),
                ("s:ads", F, b"5"),
                ("Case/x", F, b"6"),
                ("case/y", F, b"7"),
            ])
            .await;
        let r = checkout(&fx.wc(), fx.empty(), to, Mode::Safe)
            .await
            .unwrap();
        let mut skipped: Vec<_> = r.skipped.iter().map(|p| p.to_string()).collect();
        skipped.sort();
        assert_eq!(skipped, ["aux", "case/y", "dir/nul.txt", "dot.", "s:ads"]);
        assert_eq!(fx.read("ok"), b"1");
        assert_eq!(fx.read("Case/x"), b"6");
    }
}
