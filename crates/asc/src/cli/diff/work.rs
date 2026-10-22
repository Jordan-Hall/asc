//! Sides of a diff that live on disk: the work tree against the index (SPEC §9.5) and plain
//! files for `--no-index`. A file is hashed only when the synthetic index's stat data does not
//! vouch for it, and its bytes are read only when the patch needs them.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::git::blob_oid;
use asc_core::id::{FileId, GitOid};
use asc_core::object::{Entry, Kind};
use asc_core::path::RepoPath;
use asc_engine::diff::{self, ContentSource, MAX_READ_FILE_BYTES};
use asc_git::index::{IndexEntry, IndexFile, PlainStat, StatSource};
use asc_wc::SafeRoot;
use asc_wc::status::Eol;

use crate::cli::add::convert::{blob_oid as file_blob_oid, exec_bit, link_target};
use crate::cli::common::CliEngine;

/// Bytes 8.. of the id of an entry that lives on disk. Object ids are hashes, so a real one
/// never carries this tail.
const LOCAL_TAG: [u8; 24] = [0xC6; 24];

/// Content for a diff whose sides are partly on disk: entries made by [`LocalFiles::add`] read
/// from their file, every other entry from the engine.
pub struct LocalFiles<'a> {
    engine: Option<&'a CliEngine>,
    files: Vec<PathBuf>,
}

impl<'a> LocalFiles<'a> {
    pub fn new(engine: &'a CliEngine) -> Self {
        Self { engine: Some(engine), files: Vec::new() }
    }

    /// Only files on disk (`--no-index` outside a repository).
    pub fn detached() -> Self {
        Self { engine: None, files: Vec::new() }
    }

    /// An entry for the file at `abs`; its content is read from there on demand.
    pub fn add(&mut self, path: &RepoPath, abs: PathBuf, kind: Kind, size: u64, git: GitOid) -> Entry {
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(&(self.files.len() as u64).to_le_bytes());
        id[8..].copy_from_slice(&LOCAL_TAG);
        self.files.push(abs);
        Entry { name: path.file_name(), kind, id, size, git }
    }

    fn slot(&self, entry: &Entry) -> Option<&Path> {
        if entry.id[8..] != LOCAL_TAG {
            return None;
        }
        let at = u64::from_le_bytes(entry.id[..8].try_into().ok()?);
        self.files.get(usize::try_from(at).ok()?).map(PathBuf::as_path)
    }
}

/// Whether `entry` was made by [`LocalFiles::add`] (its blob id is computed, not stored).
pub fn is_local(entry: &Entry) -> bool {
    entry.id[8..] == LOCAL_TAG
}

fn io_err(e: io::Error) -> diff::Error {
    diff::Error::Io(e)
}

/// The bytes at `abs`: a link's target, else the file.
async fn read_local(abs: &Path) -> Result<Bytes, diff::Error> {
    let meta = tokio::fs::symlink_metadata(abs).await.map_err(io_err)?;
    if meta.file_type().is_symlink() {
        return link_target(abs).map(Bytes::from).map_err(io_err);
    }
    if meta.len() > MAX_READ_FILE_BYTES {
        return Err(io_err(io::Error::other(format!("{} is over the read limit", abs.display()))));
    }
    tokio::fs::read(abs).await.map(Bytes::from).map_err(io_err)
}

#[async_trait]
impl ContentSource for LocalFiles<'_> {
    async fn read(&self, entry: &Entry) -> diff::Result<Bytes> {
        match self.slot(entry) {
            Some(abs) => read_local(abs).await,
            None => match self.engine {
                Some(engine) => ContentSource::read(engine, entry).await,
                None => Err(diff::Error::MissingFile(FileId::from_bytes(entry.id))),
            },
        }
    }
}

/// What the work tree holds at a tracked path.
pub enum Work {
    /// Nothing, or a directory, or a path behind a link.
    Missing,
    /// The path is the entry itself.
    Same,
    /// Different content, mode or type.
    Changed { kind: Kind, size: u64, git: GitOid, abs: PathBuf },
}

/// Compares tracked paths with the work tree.
pub struct WorkTree {
    gate: SafeRoot,
    stat: PlainStat,
    cache: HashMap<Vec<u8>, IndexEntry>,
    index_mtime: Option<SystemTime>,
    symlinks: bool,
}

impl WorkTree {
    pub fn new(root: &Path, index_path: &Path, symlinks: bool) -> io::Result<Self> {
        let index = IndexFile::read_or_empty(index_path).map_err(io::Error::other)?;
        let cache = index.entries.into_iter().map(|e| (e.path.as_bytes().to_vec(), e)).collect();
        Ok(Self {
            gate: SafeRoot::new(root),
            stat: PlainStat { root: root.to_owned() },
            cache,
            index_mtime: index.mtime,
            symlinks,
        })
    }

    /// Whether the stat data of the index entry for `path` vouches for the file at `abs`.
    fn vouched(&self, path: &RepoPath, entry: &Entry) -> bool {
        let Some(e) = self.cache.get(path.as_bytes()) else { return false };
        if e.oid != entry.git || e.mtime.secs == 0 {
            return false;
        }
        let fresh = self.stat.stat(path, entry);
        fresh.mtime == e.mtime
            && fresh.size == e.size
            && fresh.ino == e.ino
            && self.index_mtime.is_none_or(|t| e.mtime.to_system_time() < t)
    }

    /// The work tree state of `path`, which the index holds as `entry`.
    pub fn probe(&self, path: &RepoPath, entry: &Entry) -> io::Result<Work> {
        let Some(abs) = self.gate.resolve(path) else { return Ok(Work::Missing) };
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
            Err(_) => return Ok(Work::Missing),
        };
        if meta.is_dir() {
            return Ok(Work::Missing);
        }
        let is_link = meta.file_type().is_symlink();
        if !is_link && !meta.is_file() {
            return Ok(Work::Same); // FIFOs and devices are never read
        }
        // Without symlink support a link is a regular file holding its target.
        let as_link = is_link || (entry.kind == Kind::Symlink && !self.symlinks);
        let exec = if as_link { false } else { exec_bit(&meta).unwrap_or(entry.kind == Kind::Exec) };
        let kind = if as_link {
            Kind::Symlink
        } else if exec {
            Kind::Exec
        } else {
            Kind::File
        };
        if !is_link && kind == entry.kind && self.vouched(path, entry) {
            return Ok(Work::Same);
        }
        let git = if is_link {
            blob_oid(&link_target(&abs)?)
        } else {
            file_blob_oid(&abs, Eol::Keep)?
        };
        if kind == entry.kind && git == entry.git {
            return Ok(Work::Same);
        }
        Ok(Work::Changed { kind, size: meta.len(), git, abs })
    }
}
