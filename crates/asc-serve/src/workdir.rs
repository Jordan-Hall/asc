//! Private working directories for agent runs and workflow steps.
//!
//! Commands of a run write into these directories and a repository chooses what lands in them, so
//! they are never shared: the default is a directory under the server's data directory (the
//! cache disk directory, else the directory of the SQLite database), every directory is created
//! with mode 0700, and [`prepare`] refuses one that the server's user does not own, that is a
//! link, or that other users can write to. Files of a repository are written without following
//! links ([`write_new`]).

use std::io;
use std::path::{Path, PathBuf};

use crate::config::ServeConfig;

/// The directory used when `work_dir` is unset: `<data dir>/<leaf>`. With no data directory (an
/// in-memory database and no cache directory) it is `<temp>/asc-<leaf>-<uid>`, which [`prepare`]
/// still checks for ownership and mode.
#[must_use]
pub fn default_dir(cfg: &ServeConfig, leaf: &str) -> PathBuf {
    match data_dir(cfg) {
        Some(d) => d.join(leaf),
        None => fallback(leaf),
    }
}

/// `<temp>/asc-<leaf>-<uid>`: a name no other user can have taken for themselves.
#[must_use]
pub fn fallback(leaf: &str) -> PathBuf {
    std::env::temp_dir().join(format!("asc-{leaf}-{}", uid()))
}

fn data_dir(cfg: &ServeConfig) -> Option<PathBuf> {
    if let Some(d) = &cfg.cache.disk_dir {
        return Some(d.clone());
    }
    sqlite_file(cfg.db.as_deref()?)?
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// The file a `sqlite:` database URL names; `None` for memory databases and other schemes.
#[must_use]
pub fn sqlite_file(db: &str) -> Option<PathBuf> {
    let rest = db.strip_prefix("sqlite:")?;
    let file = rest.trim_start_matches("//").split('?').next()?;
    if file.is_empty() || file.starts_with(':') {
        return None;
    }
    Some(PathBuf::from(file))
}

/// Creates `dir` and its parents; `dir` itself gets mode 0700.
///
/// # Errors
/// The directory cannot be made.
pub fn create_private_all(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    match create_private(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => {}
        other => return other,
    }
    Ok(())
}

#[cfg(unix)]
fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// Creates `dir` (and its parents) when missing, with mode 0700 for `dir`, then checks it: a real
/// directory, owned by this process's user, writable by no one else.
///
/// # Errors
/// The directory cannot be made, or it fails the check; the message names the path and the rule.
pub fn prepare(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    match create_private(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check(dir)
}

/// Creates one directory with mode 0700. It fails when it exists.
///
/// # Errors
/// The directory exists or cannot be made.
pub fn create_private(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

#[cfg(unix)]
fn check(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let denied = |why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("work directory {}: {why}", dir.display()),
        )
    };
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(denied("it is a symbolic link"));
    }
    if !meta.is_dir() {
        return Err(denied("it is not a directory"));
    }
    if meta.uid() != uid() {
        return Err(denied("it is owned by another user"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(denied(&format!(
            "mode {:04o} lets group or others write to it; chmod 700",
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check(dir: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("work directory {}: not a plain directory", dir.display()),
        ));
    }
    Ok(())
}

/// Writes `data` to a new file at `path`. The file must not exist and the last component must not
/// be a link (`O_NOFOLLOW`, `O_EXCL`), so a path cannot be redirected out of the tree.
///
/// # Errors
/// The file exists, is a link, or cannot be written.
pub async fn write_new(path: &Path, data: &[u8], exec: bool) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut o = tokio::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        o.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        o.mode(if exec { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    let _ = exec;
    let mut f = o.open(path).await?;
    f.write_all(data).await?;
    f.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_under_the_data_directory() {
        let mut cfg = ServeConfig {
            db: Some("sqlite:/srv/asc/state/asc.db?mode=rwc".into()),
            ..ServeConfig::default()
        };
        assert_eq!(
            default_dir(&cfg, "agents"),
            PathBuf::from("/srv/asc/state/agents")
        );
        cfg.cache.disk_dir = Some("/var/cache/asc".into());
        assert_eq!(
            default_dir(&cfg, "workflows"),
            PathBuf::from("/var/cache/asc/workflows")
        );
        cfg.cache.disk_dir = None;
        cfg.db = Some("sqlite::memory:".into());
        let f = default_dir(&cfg, "agents");
        assert!(f.starts_with(std::env::temp_dir()), "{f:?}");
        assert!(
            f.to_string_lossy().ends_with(&format!("-{}", uid())),
            "{f:?}"
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::PermissionsExt as _;

        use super::super::*;

        #[test]
        fn a_new_directory_is_private_and_a_loose_one_is_refused() {
            let t = tempfile::tempdir().unwrap();
            let d = t.path().join("work");
            prepare(&d).unwrap();
            let mode = std::fs::metadata(&d).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            prepare(&d).expect("an existing private directory is fine");
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o770)).unwrap();
            let e = prepare(&d).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert!(e.to_string().contains("chmod 700"), "{e}");
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o702)).unwrap();
            assert!(prepare(&d).is_err(), "world-writable");
        }

        #[test]
        fn a_link_and_a_file_are_not_work_directories() {
            let t = tempfile::tempdir().unwrap();
            let real = t.path().join("real");
            prepare(&real).unwrap();
            let link = t.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(prepare(&link).unwrap_err().to_string().contains("link"));
            let file = t.path().join("file");
            std::fs::write(&file, b"x").unwrap();
            assert!(prepare(&file).is_err());
        }

        #[tokio::test]
        async fn a_file_is_never_written_through_a_link() {
            let t = tempfile::tempdir().unwrap();
            let outside = t.path().join("outside");
            std::fs::write(&outside, b"keep").unwrap();
            let link = t.path().join("tree-file");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(write_new(&link, b"overwritten", false).await.is_err());
            assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
            let fresh = t.path().join("fresh");
            write_new(&fresh, b"data", true).await.unwrap();
            assert_eq!(
                std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert!(
                write_new(&fresh, b"again", false).await.is_err(),
                "no overwrite"
            );
        }
    }
}
