//! Git index file (SPEC §9.4, §9.5): the synthetic turbo export, asc's stat cache
//! and temporary stage-set files for `GIT_INDEX_FILE`.
//!
//! Reading and entry encoding go through `gix-index`; the TREE extension is
//! spliced in here because `gix-index` cannot set it. [`render`] and [`write`]
//! are the only way asc produces an index (git's `index.lock` protocol).

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asc_core::git::{ObjectKind, object_oid};
use asc_core::id::GitOid;
use asc_core::object::{Entry, Kind, Tree};
use asc_core::path::RepoPath;
use gix_index::entry::stat::Time as GixTime;
use gix_index::entry::{Flags, Mode, Stat as GixStat};
use sha1::{Digest, Sha1};

use crate::odb::{from_gix, to_gix};

/// How long [`write`] waits for a held `index.lock`.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Margin added to the newest entry time when choosing the index mtime.
const RACY_MARGIN: Duration = Duration::from_secs(2);

/// Why an index could not be read, written or locked.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("corrupt index: {0}")]
    Decode(String),
    #[error("cannot encode index: {0}")]
    Encode(String),
    #[error("index path is not a valid repository path: {0}")]
    InvalidPath(String),
    #[error("index entry has unsupported mode {0:o}")]
    UnsupportedMode(u32),
    #[error(transparent)]
    Lock(#[from] LockError),
}

/// Why `index.lock` could not be taken. `Exists` carries git's message.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "Unable to create '{}': File exists.\n\n\
         Another git process seems to be running in this repository, e.g.\n\
         an editor opened by 'git commit'. Please make sure all processes\n\
         are terminated then try again. If it still fails, a git process\n\
         may have crashed in this repository earlier:\n\
         remove the file manually to continue.",
        path.display()
    )]
    Exists { path: PathBuf },
    #[error("Unable to create '{}': {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> IndexError + '_ {
    move |source| IndexError::Io {
        path: path.to_owned(),
        source,
    }
}

/// A stat time as stored in the index (seconds and nanoseconds since the epoch).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatTime {
    pub secs: u32,
    pub nsecs: u32,
}

impl StatTime {
    /// Clamps times before 1970 to zero and after 2106 to the maximum.
    pub fn from_system_time(t: SystemTime) -> Self {
        let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            secs: u32::try_from(d.as_secs()).unwrap_or(u32::MAX),
            nsecs: d.subsec_nanos(),
        }
    }

    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::new(self.secs.into(), self.nsecs)
    }
}

/// Entry file mode. Directories never appear in an asc-written index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMode {
    File,
    Exec,
    Symlink,
    Gitlink,
}

impl FileMode {
    fn from_kind(kind: Kind) -> Option<Self> {
        Some(match kind {
            Kind::File => Self::File,
            Kind::Exec => Self::Exec,
            Kind::Symlink => Self::Symlink,
            Kind::Gitlink => Self::Gitlink,
            Kind::Dir => return None,
        })
    }

    fn from_bits(bits: u32) -> Result<Self, IndexError> {
        match bits & 0o170000 {
            0o100000 if bits & 0o100 != 0 => Ok(Self::Exec),
            0o100000 => Ok(Self::File),
            0o120000 => Ok(Self::Symlink),
            0o160000 => Ok(Self::Gitlink),
            _ => Err(IndexError::UnsupportedMode(bits)),
        }
    }

    fn to_gix(self) -> Mode {
        match self {
            Self::File => Mode::FILE,
            Self::Exec => Mode::FILE_EXECUTABLE,
            Self::Symlink => Mode::SYMLINK,
            Self::Gitlink => Mode::COMMIT,
        }
    }

    /// The octal mode as git prints and hashes it.
    fn tree_mode(self) -> &'static [u8] {
        match self {
            Self::File => b"100644",
            Self::Exec => b"100755",
            Self::Symlink => b"120000",
            Self::Gitlink => b"160000",
        }
    }
}

/// Merge stage of an entry; `Normal` outside conflicts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    #[default]
    Normal,
    Base,
    Ours,
    Theirs,
}

impl Stage {
    fn to_gix(self) -> gix_index::entry::Stage {
        match self {
            Self::Normal => gix_index::entry::Stage::Unconflicted,
            Self::Base => gix_index::entry::Stage::Base,
            Self::Ours => gix_index::entry::Stage::Ours,
            Self::Theirs => gix_index::entry::Stage::Theirs,
        }
    }

    fn from_gix(s: gix_index::entry::Stage) -> Self {
        match s {
            gix_index::entry::Stage::Unconflicted => Self::Normal,
            gix_index::entry::Stage::Base => Self::Base,
            gix_index::entry::Stage::Ours => Self::Ours,
            gix_index::entry::Stage::Theirs => Self::Theirs,
        }
    }
}

/// Per-entry flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryFlags {
    pub stage: Stage,
    pub skip_worktree: bool,
    pub intent_to_add: bool,
}

/// One index entry. `size` is truncated to 32 bits, as in git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub path: RepoPath,
    pub mode: FileMode,
    pub oid: GitOid,
    pub size: u32,
    pub ctime: StatTime,
    pub mtime: StatTime,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub flags: EntryFlags,
}

/// A node of the index's TREE extension (git's cache-tree). `oid` is `None`
/// when the node is invalidated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheTree {
    /// Directory name; empty for the root.
    pub name: Vec<u8>,
    pub oid: Option<GitOid>,
    /// Index entries below this directory, recursively.
    pub entries: u32,
    /// Child directories, ascending by name.
    pub children: Vec<CacheTree>,
}

impl CacheTree {
    /// Cache-tree of an asc tree from the `git` fields of its directories.
    /// `load` returns the subtree for a `Dir` entry; `root_oid` is the git oid
    /// of `root`.
    pub fn from_asc_tree<E>(
        root: &Tree,
        root_oid: GitOid,
        load: &mut dyn FnMut(&Entry) -> Result<Tree, E>,
    ) -> Result<Self, E> {
        fn node<E>(
            name: Vec<u8>,
            oid: GitOid,
            tree: &Tree,
            load: &mut dyn FnMut(&Entry) -> Result<Tree, E>,
        ) -> Result<CacheTree, E> {
            let (mut entries, mut children) = (0, Vec::new());
            for e in tree.entries() {
                if e.kind == Kind::Dir {
                    let sub = load(e)?;
                    let child = node(e.name.as_bytes().to_vec(), e.git, &sub, load)?;
                    entries += child.entries;
                    children.push(child);
                } else {
                    entries += 1;
                }
            }
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(CacheTree {
                name,
                oid: Some(oid),
                entries,
                children,
            })
        }
        node(Vec::new(), root_oid, root, load)
    }

    /// Computes the cache-tree of stage-0 entries sorted by path.
    pub fn from_entries(entries: &[IndexEntry]) -> Self {
        build_tree(entries, 0, Vec::new())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name);
        out.push(0);
        match self.oid {
            Some(_) => out.extend_from_slice(self.entries.to_string().as_bytes()),
            None => out.extend_from_slice(b"-1"),
        }
        out.push(b' ');
        out.extend_from_slice(self.children.len().to_string().as_bytes());
        out.push(b'\n');
        if let Some(oid) = self.oid {
            out.extend_from_slice(oid.as_bytes());
        }
        for c in &self.children {
            c.encode(out);
        }
    }

    fn from_gix(t: &gix_index::extension::Tree) -> Self {
        Self {
            name: t.name.to_vec(),
            oid: t.num_entries.and_then(|_| from_gix(&t.id)),
            entries: t.num_entries.unwrap_or(0),
            children: t.children.iter().map(Self::from_gix).collect(),
        }
    }
}

fn build_tree(entries: &[IndexEntry], prefix: usize, name: Vec<u8>) -> CacheTree {
    let (mut body, mut children, mut count) = (Vec::new(), Vec::new(), 0u32);
    let mut i = 0;
    while i < entries.len() {
        let rest = &entries[i].path.as_bytes()[prefix..];
        if let Some(p) = rest.iter().position(|&c| c == b'/') {
            let comp = &rest[..p];
            let run = entries[i..].partition_point(|e| {
                let r = &e.path.as_bytes()[prefix..];
                r.len() > p && &r[..p] == comp && r[p] == b'/'
            });
            let child = build_tree(&entries[i..i + run], prefix + p + 1, comp.to_vec());
            body.extend_from_slice(b"40000 ");
            body.extend_from_slice(comp);
            body.push(0);
            body.extend_from_slice(child.oid.expect("computed").as_bytes());
            count += child.entries;
            children.push(child);
            i += run;
        } else {
            body.extend_from_slice(entries[i].mode.tree_mode());
            body.push(b' ');
            body.extend_from_slice(rest);
            body.push(0);
            body.extend_from_slice(entries[i].oid.as_bytes());
            count += 1;
            i += 1;
        }
    }
    children.sort_by(|a: &CacheTree, b| a.name.cmp(&b.name));
    CacheTree {
        name,
        oid: Some(object_oid(ObjectKind::Tree, &body)),
        entries: count,
        children,
    }
}

/// A parsed index file (v2/v3; only the TREE extension is kept).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexFile {
    /// Ascending by path, then stage.
    pub entries: Vec<IndexEntry>,
    pub tree: Option<CacheTree>,
    /// The mtime the file gets when written; on read, the file's own mtime.
    pub mtime: Option<SystemTime>,
}

impl IndexFile {
    /// Reads and checksum-verifies the index at `path`.
    pub fn read(path: &Path) -> Result<Self, IndexError> {
        let data = std::fs::read(path).map_err(io_err(path))?;
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(io_err(path))?;
        Self::from_bytes(&data, mtime)
    }

    /// Like [`read`](Self::read), but a missing file is an empty index.
    pub fn read_or_empty(path: &Path) -> Result<Self, IndexError> {
        match Self::read(path) {
            Err(IndexError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            other => other,
        }
    }

    /// Parses index bytes; `mtime` is recorded as [`IndexFile::mtime`].
    pub fn from_bytes(data: &[u8], mtime: SystemTime) -> Result<Self, IndexError> {
        let ft = filetime::FileTime::from_system_time(mtime);
        let (state, _) = gix_index::State::from_bytes(
            data,
            ft,
            gix::hash::Kind::Sha1,
            gix_index::decode::Options::default(),
        )
        .map_err(|e| IndexError::Decode(e.to_string()))?;
        let mut entries = Vec::with_capacity(state.entries().len());
        for e in state.entries() {
            let path = e.path(&state);
            let path = RepoPath::new(path.to_vec())
                .map_err(|_| IndexError::InvalidPath(path.to_string()))?;
            let oid = from_gix(&e.id).ok_or_else(|| IndexError::Decode("non-SHA-1 id".into()))?;
            let time = |t: GixTime| StatTime {
                secs: t.secs,
                nsecs: t.nsecs,
            };
            entries.push(IndexEntry {
                path,
                mode: FileMode::from_bits(e.mode.bits())?,
                oid,
                size: e.stat.size,
                ctime: time(e.stat.ctime),
                mtime: time(e.stat.mtime),
                dev: e.stat.dev,
                ino: e.stat.ino,
                uid: e.stat.uid,
                gid: e.stat.gid,
                flags: EntryFlags {
                    stage: Stage::from_gix(e.stage()),
                    skip_worktree: e.flags.contains(Flags::SKIP_WORKTREE),
                    intent_to_add: e.flags.contains(Flags::INTENT_TO_ADD),
                },
            });
        }
        Ok(Self {
            entries,
            tree: state.tree().map(CacheTree::from_gix),
            mtime: Some(mtime),
        })
    }

    /// Serialises to the on-disk format, with the TREE extension and checksum.
    pub fn to_bytes(&self) -> Result<Vec<u8>, IndexError> {
        let mut state = gix_index::State::new(gix::hash::Kind::Sha1);
        for e in &self.entries {
            let mut flags = Flags::from_stage(e.flags.stage.to_gix());
            flags.set(Flags::SKIP_WORKTREE, e.flags.skip_worktree);
            flags.set(Flags::INTENT_TO_ADD, e.flags.intent_to_add);
            flags.set(
                Flags::EXTENDED,
                e.flags.skip_worktree || e.flags.intent_to_add,
            );
            let time = |t: StatTime| GixTime {
                secs: t.secs,
                nsecs: t.nsecs,
            };
            let stat = GixStat {
                mtime: time(e.mtime),
                ctime: time(e.ctime),
                dev: e.dev,
                ino: e.ino,
                uid: e.uid,
                gid: e.gid,
                size: e.size,
            };
            state.dangerously_push_entry(
                stat,
                to_gix(e.oid),
                flags,
                e.mode.to_gix(),
                e.path.as_bytes().into(),
            );
        }
        state.sort_entries();
        let mut out = Vec::new();
        state
            .write_to(
                &mut out,
                gix_index::write::Options {
                    extensions: gix_index::write::Extensions::None,
                    skip_hash: true,
                },
            )
            .map_err(|e| IndexError::Encode(e.to_string()))?;
        if let Some(tree) = &self.tree {
            let mut data = Vec::new();
            tree.encode(&mut data);
            let len = u32::try_from(data.len())
                .map_err(|_| IndexError::Encode("cache-tree too large".into()))?;
            out.extend_from_slice(b"TREE");
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&data);
        }
        let sum = Sha1::digest(&out);
        out.extend_from_slice(&sum);
        Ok(out)
    }
}

// ----------------------------------------------------------------- locking

/// An `index.lock` held under git's protocol: created exclusively, renamed over
/// the index by [`commit`](Self::commit), removed on drop otherwise.
#[derive(Debug)]
pub struct IndexLock {
    file: Option<File>,
    lock_path: PathBuf,
    target: PathBuf,
}

impl IndexLock {
    /// Locks `<gitdir>/index`, retrying until `timeout`.
    pub fn acquire(gitdir: &Path, timeout: Duration) -> Result<Self, LockError> {
        Self::acquire_for(&gitdir.join("index"), timeout)
    }

    /// Locks the index file at `index_path` via `<index_path>.lock`.
    pub fn acquire_for(index_path: &Path, timeout: Duration) -> Result<Self, LockError> {
        let mut name = index_path.as_os_str().to_owned();
        name.push(".lock");
        let lock_path = PathBuf::from(name);
        let start = Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        lock_path,
                        target: index_path.to_owned(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() >= timeout {
                        return Err(LockError::Exists { path: lock_path });
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(source) => {
                    return Err(LockError::Io {
                        path: lock_path,
                        source,
                    });
                }
            }
        }
    }

    /// The path being written (`index.lock`).
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Writes bytes to the lock file.
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file().write_all(bytes)
    }

    /// Sets the mtime the index will have once committed. Call after the last write.
    pub fn set_index_mtime(&mut self, t: SystemTime) -> std::io::Result<()> {
        filetime::set_file_handle_times(self.file(), None, Some(t.into()))
    }

    /// fsyncs and renames the lock over the index.
    pub fn commit(mut self) -> std::io::Result<()> {
        let file = self.file.take().expect("open until commit");
        file.sync_all()?;
        drop(file);
        std::fs::rename(&self.lock_path, &self.target)
    }

    fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("open until commit")
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }
}

/// Writes `index` to `path` under the `index.lock` protocol, applying
/// [`IndexFile::mtime`] if set. The only index writer (SPEC §9.4).
pub fn write(path: &Path, index: &IndexFile) -> Result<(), IndexError> {
    let bytes = index.to_bytes()?;
    let mut lock = IndexLock::acquire_for(path, LOCK_TIMEOUT)?;
    let lock_path = lock.lock_path().to_owned();
    lock.write_all(&bytes).map_err(io_err(&lock_path))?;
    if let Some(t) = index.mtime {
        lock.set_index_mtime(t).map_err(io_err(&lock_path))?;
    }
    lock.commit().map_err(io_err(path))
}

// ------------------------------------------------------------------ render

/// Stat data for one entry, as stored in the index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStat {
    pub ctime: StatTime,
    pub mtime: StatTime,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

/// Where [`render`] gets stat data: real stat for a plain working copy, the
/// SPEC §10.1 times for a virtual one.
pub trait StatSource {
    fn stat(&self, path: &RepoPath, entry: &Entry) -> FileStat;
}

/// Real `lstat` of files under `root`. A missing file gets zero times, so it
/// reads as stat-dirty.
#[derive(Clone, Debug)]
pub struct PlainStat {
    pub root: PathBuf,
}

impl StatSource for PlainStat {
    fn stat(&self, path: &RepoPath, entry: &Entry) -> FileStat {
        let rel = String::from_utf8_lossy(path.as_bytes()).into_owned();
        let Ok(meta) = std::fs::symlink_metadata(self.root.join(rel)) else {
            return FileStat {
                size: entry.size as u32,
                ..FileStat::default()
            };
        };
        let mtime = meta
            .modified()
            .map(StatTime::from_system_time)
            .unwrap_or_default();
        #[cfg(unix)]
        let (ctime, dev, ino, uid, gid) = {
            use std::os::unix::fs::MetadataExt;
            (
                StatTime {
                    secs: u32::try_from(meta.ctime()).unwrap_or(0),
                    nsecs: u32::try_from(meta.ctime_nsec()).unwrap_or(0),
                },
                meta.dev() as u32,
                meta.ino() as u32,
                meta.uid(),
                meta.gid(),
            )
        };
        #[cfg(not(unix))]
        let (ctime, dev, ino, uid, gid) = (
            meta.created()
                .map(StatTime::from_system_time)
                .unwrap_or(mtime),
            0,
            0,
            0,
            0,
        );
        FileStat {
            ctime,
            mtime,
            dev,
            ino,
            uid,
            gid,
            size: meta.len() as u32,
        }
    }
}

/// Options for [`render`].
pub struct RenderOptions<'a> {
    /// The clock; `None` reads the system clock.
    pub now: Option<SystemTime>,
    /// Build the TREE extension (default on).
    pub cache_tree: bool,
    /// Marks paths outside a sparse cone with skip-worktree.
    pub skip_worktree: Option<&'a dyn Fn(&RepoPath) -> bool>,
}

impl Default for RenderOptions<'_> {
    fn default() -> Self {
        Self {
            now: None,
            cache_tree: true,
            skip_worktree: None,
        }
    }
}

/// Builds an index from tree entries. `entries` are files, symlinks and
/// gitlinks (`Dir` entries are ignored); order does not matter. Gitlinks get
/// zero stat. The index mtime is `max(now, newest entry time + 2 s)` so no
/// entry reads as racily clean.
pub fn render(
    entries: impl Iterator<Item = (RepoPath, Entry)>,
    stat: &dyn StatSource,
    opts: &RenderOptions<'_>,
) -> IndexFile {
    let mut newest = StatTime::default();
    let mut out: Vec<IndexEntry> = entries
        .filter_map(|(path, e)| {
            let mode = FileMode::from_kind(e.kind)?;
            let st = if mode == FileMode::Gitlink {
                FileStat::default()
            } else {
                stat.stat(&path, &e)
            };
            newest = newest.max(st.ctime).max(st.mtime);
            let skip_worktree = opts.skip_worktree.is_some_and(|f| f(&path));
            Some(IndexEntry {
                path,
                mode,
                oid: e.git,
                size: st.size,
                ctime: st.ctime,
                mtime: st.mtime,
                dev: st.dev,
                ino: st.ino,
                uid: st.uid,
                gid: st.gid,
                flags: EntryFlags {
                    skip_worktree,
                    ..EntryFlags::default()
                },
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    let now = opts.now.unwrap_or_else(SystemTime::now);
    let mtime = now.max(newest.to_system_time() + RACY_MARGIN);
    IndexFile {
        tree: opts.cache_tree.then(|| CacheTree::from_entries(&out)),
        entries: out,
        mtime: Some(mtime),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::git::blob_oid;
    use asc_core::path::Name;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        // lint:allow-raw-git
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).expect("utf8")
    }

    struct Fixed;
    impl StatSource for Fixed {
        fn stat(&self, _: &RepoPath, e: &Entry) -> FileStat {
            let t = StatTime {
                secs: 1_700_000_000,
                nsecs: 5,
            };
            FileStat {
                ctime: t,
                mtime: t,
                size: e.size as u32,
                ..FileStat::default()
            }
        }
    }

    fn entry(name: &str, kind: Kind, content: &[u8]) -> (RepoPath, Entry) {
        let path = RepoPath::new(name).expect("path");
        let e = Entry {
            name: Name::new(name.rsplit('/').next().expect("name")).expect("name"),
            kind,
            id: [0; 32],
            size: content.len() as u64,
            git: blob_oid(content),
        };
        (path, e)
    }

    const FILES: &[(&str, &[u8])] = &[
        ("a.txt", b"one\n"),
        ("a/b.txt", b"two\n"),
        ("a-b/c.txt", b"three\n"),
        ("z/y/x.txt", b"four\n"),
    ];

    /// A repo with the files committed by git; returns it with the work tree populated.
    fn committed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "user.email", "t@example.com"]);
        for (name, body) in FILES {
            let f = p.join(name);
            std::fs::create_dir_all(f.parent().expect("parent")).expect("mkdir");
            std::fs::write(f, body).expect("write");
        }
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "init"]);
        dir
    }

    fn render_files(root: &Path) -> IndexFile {
        let entries = FILES.iter().map(|(n, b)| entry(n, Kind::File, b));
        render(
            entries,
            &PlainStat {
                root: root.to_owned(),
            },
            &RenderOptions::default(),
        )
    }

    #[test]
    fn written_index_is_accepted_by_git() {
        let dir = committed_repo();
        let p = dir.path();
        let idx = render_files(p);
        write(&p.join(".git/index"), &idx).expect("write");
        let ls = git(p, &["ls-files", "-s"]);
        let expected: Vec<String> = ["a-b/c.txt", "a.txt", "a/b.txt", "z/y/x.txt"]
            .iter()
            .map(|n| {
                let body = FILES.iter().find(|f| f.0 == *n).expect("file").1;
                format!("100644 {} 0\t{n}", blob_oid(body))
            })
            .collect();
        assert_eq!(ls.lines().collect::<Vec<_>>(), expected);
        assert_eq!(git(p, &["status", "--porcelain"]), "");
        let root = idx.tree.as_ref().and_then(|t| t.oid).expect("root");
        assert_eq!(
            git(p, &["rev-parse", "HEAD^{tree}"]).trim(),
            root.to_string()
        );
        assert_eq!(git(p, &["fsck", "--no-dangling"]), "");
    }

    #[test]
    fn cache_tree_is_used_by_git_write_tree() {
        let dir = committed_repo();
        let p = dir.path();
        write(&p.join(".git/index"), &render_files(p)).expect("write");
        let tree = git(p, &["write-tree"]);
        assert_eq!(tree.trim(), git(p, &["rev-parse", "HEAD^{tree}"]).trim());
    }

    #[test]
    fn git_written_index_round_trips() {
        let dir = committed_repo();
        let p = dir.path();
        let path = p.join(".git/index");
        let read = IndexFile::read(&path).expect("read");
        assert_eq!(read.entries.len(), FILES.len());
        let tree = read.tree.clone().expect("git writes TREE");
        assert_eq!(
            tree.oid.expect("valid").to_string(),
            git(p, &["rev-parse", "HEAD^{tree}"]).trim()
        );
        let again = IndexFile::from_bytes(&read.to_bytes().expect("encode"), SystemTime::now())
            .expect("decode");
        assert_eq!(again.entries, read.entries);
        assert_eq!(again.tree, read.tree);
        write(&path, &read).expect("write");
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn flags_and_modes_round_trip() {
        let mut idx = render(
            [
                entry("exec.sh", Kind::Exec, b"#!/bin/sh\n"),
                entry("link", Kind::Symlink, b"exec.sh"),
                entry("sub", Kind::Gitlink, b""),
            ]
            .into_iter(),
            &Fixed,
            &RenderOptions {
                skip_worktree: Some(&|p| p.as_bytes() == b"link"),
                ..RenderOptions::default()
            },
        );
        idx.entries[0].flags.intent_to_add = true;
        let back = IndexFile::from_bytes(&idx.to_bytes().expect("encode"), SystemTime::now())
            .expect("decode");
        assert_eq!(back.entries, idx.entries);
        let modes: Vec<_> = back.entries.iter().map(|e| e.mode).collect();
        assert_eq!(
            modes,
            [FileMode::Exec, FileMode::Symlink, FileMode::Gitlink]
        );
        assert!(back.entries[0].flags.intent_to_add);
        assert!(back.entries[1].flags.skip_worktree);
    }

    #[test]
    fn index_mtime_clears_the_racy_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let idx = render(
            [entry("f", Kind::File, b"x")].into_iter(),
            &Fixed,
            &RenderOptions {
                now: Some(now),
                ..RenderOptions::default()
            },
        );
        assert_eq!(
            idx.mtime,
            Some(UNIX_EPOCH + Duration::new(1_700_000_002, 5))
        );
        let path = dir.path().join("index");
        write(&path, &idx).expect("write");
        let got = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .expect("mtime");
        assert_eq!(got, idx.mtime.expect("set"));
    }

    #[test]
    fn held_lock_gives_git_message_after_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let held = IndexLock::acquire(dir.path(), Duration::ZERO).expect("first");
        let start = Instant::now();
        let err = IndexLock::acquire(dir.path(), Duration::from_millis(200)).expect_err("held");
        assert!(start.elapsed() >= Duration::from_millis(200));
        let msg = err.to_string();
        assert!(msg.starts_with("Unable to create '"), "{msg}");
        assert!(msg.contains("index.lock': File exists."), "{msg}");
        assert!(msg.contains("Another git process seems to be running in this repository"));
        drop(held);
        assert!(!dir.path().join("index.lock").exists());
        IndexLock::acquire(dir.path(), Duration::ZERO).expect("free again");
    }

    #[test]
    fn dropped_lock_leaves_index_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut lock = IndexLock::acquire(dir.path(), Duration::ZERO).expect("lock");
        lock.write_all(b"partial").expect("write");
        drop(lock);
        assert!(!dir.path().join("index").exists());
    }
}
