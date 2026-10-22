//! The minimal synthetic `.git` of a workspace (SPEC §9.4): a gitfile, a per-workspace gitdir
//! holding only `commondir`, `HEAD` and `asc-workspace`, and a shared git-format common dir with
//! `config`, `info/exclude` and `hooks/`. No objects, refs or `config.worktree`.

pub mod config;
pub mod index_export;
pub mod layout;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub use config::Config;
pub use layout::{Head, Projection, RemoteName};

/// Failures creating, discovering or configuring a synthetic `.git`.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("fatal: not an asc workspace (or any parent up to the filesystem root): {}", .0.display())]
    NotFound(PathBuf),
    #[error("{} is not an asc synthetic .git", .0.display())]
    NotWorkspace(PathBuf),
    #[error("{}: malformed gitfile or gitdir", .0.display())]
    Malformed(PathBuf),
    #[error("{} is a real git directory", .0.display())]
    RealGitDir(PathBuf),
    #[error("invalid branch name '{0}'")]
    BadBranch(String),
    #[error("invalid remote name '{0}'")]
    BadRemote(String),
    #[error("{}: config: {message}", path.display())]
    Config { path: PathBuf, message: String },
}

impl ProjectionError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }

    pub(crate) fn config(path: &Path, err: impl std::fmt::Display) -> Self {
        Self::Config {
            path: path.to_owned(),
            message: err.to_string(),
        }
    }
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ProjectionError>;

/// Writes `bytes` to `path` with git's lock protocol: `<path>.lock` created exclusively, then
/// renamed over the target.
pub(crate) fn write_locked(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&lock, path)
    })();
    if let Err(source) = written {
        // Leave a lock held by someone else alone; remove only our own failed one.
        if source.kind() != io::ErrorKind::AlreadyExists {
            let _ = std::fs::remove_file(&lock);
        }
        return Err(ProjectionError::io(path, source));
    }
    Ok(())
}
