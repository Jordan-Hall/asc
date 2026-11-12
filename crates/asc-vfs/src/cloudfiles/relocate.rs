//! Moving a stopped sync root (`worktree move` of a virtual workspace, SPEC §10.3b).
//!
//! The filter refuses to rename a registered sync root, and unregistering one reverts every
//! placeholder: a hydrated file or a listed directory becomes a plain one, and a dehydrated file
//! or a directory never listed is deleted. So the move is in four steps:
//!
//! 1. Every placeholder under the root is recorded: its path, identity, sync state, and whether
//!    a directory was listed. Directories never listed are not entered (they hold nothing).
//! 2. The root is unregistered and renamed.
//! 3. The new path is registered for the same workspace.
//! 4. The placeholders are rebuilt, parents first: what is still on disk is converted back with
//!    its identity and sync state, and what unregistering deleted is created again in sync.
//!
//! Plain files and directories (the user's) move as they are. The workspace then reads exactly
//! as before: clean files are clean, edits are edits, deletions in listed directories stay
//! deleted, and a directory never listed still lists from its own tree.

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::ffi::{self, Expose, NewPlaceholder};
use crate::win::verbatim;

/// One recorded placeholder.
#[derive(Clone, Debug)]
struct Entry {
    /// Root-relative path; empty for the root itself.
    rel: PathBuf,
    dir: bool,
    /// A directory whose children are all on disk.
    listed: bool,
    identity: Vec<u8>,
    in_sync: bool,
    size: u64,
    /// Unix seconds of the last write.
    mtime: i64,
    read_only: bool,
}

/// Moves the stopped sync root of `workspace` from `from` to `to`. A directory that is not this
/// provider's sync root is renamed as it is. When a step fails the root is put back at `from`,
/// registered and rebuilt, as far as that can be done.
pub fn relocate(from: &Path, to: &Path, workspace: &[u8; 16]) -> io::Result<()> {
    if !ffi::is_our_sync_root(from) {
        return std::fs::rename(from, to);
    }
    if ffi::sync_root_identity(from).as_deref() != Some(&workspace[..]) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is the sync root of another workspace", from.display()),
        ));
    }
    let entries = snapshot(from)?;
    ffi::unregister(from)?;
    let back = |e: io::Error| {
        let _ = adopt(from, workspace, &entries);
        Err(e)
    };
    if let Err(e) = std::fs::rename(from, to) {
        return back(e);
    }
    if let Err(e) = adopt(to, workspace, &entries) {
        let _ = ffi::unregister(to);
        if std::fs::rename(to, from).is_err() {
            return Err(e);
        }
        return back(e);
    }
    Ok(())
}

/// Every placeholder under `root`, the root first and each directory before its children.
fn snapshot(root: &Path) -> io::Result<Vec<Entry>> {
    let _expose = Expose::new();
    let base = verbatim(root);
    let mut out = vec![record(&base, PathBuf::new())?];
    let mut stack = vec![PathBuf::new()];
    while let Some(dir) = stack.pop() {
        for item in std::fs::read_dir(base.join(&dir))? {
            let rel = dir.join(item?.file_name());
            let abs = base.join(&rel);
            let probe = ffi::probe(&abs)?;
            if probe.is_foreign_reparse() {
                continue;
            }
            if probe.is_placeholder() {
                out.push(record(&abs, rel.clone())?);
            }
            if probe.is_dir() && !probe.is_unpopulated() {
                stack.push(rel);
            }
        }
    }
    Ok(out)
}

fn record(abs: &Path, rel: PathBuf) -> io::Result<Entry> {
    let probe = ffi::probe(abs)?;
    let meta = std::fs::symlink_metadata(abs)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    Ok(Entry {
        rel,
        dir: probe.is_dir(),
        listed: probe.is_dir() && !probe.is_unpopulated(),
        identity: probe.identity.clone(),
        in_sync: probe.in_sync,
        size: if probe.is_dir() { 0 } else { meta.len() },
        mtime,
        read_only: probe.attrs & ffi::FILE_ATTRIBUTE_READONLY != 0,
    })
}

/// Registers `root` for `workspace` and rebuilds the placeholders `entries` recorded.
fn adopt(root: &Path, workspace: &[u8; 16], entries: &[Entry]) -> io::Result<()> {
    ffi::register(root, workspace)?;
    let base = verbatim(root);
    for e in entries {
        if e.rel.as_os_str().is_empty() {
            if e.listed {
                ffi::disable_on_demand(&base)?;
            }
            continue;
        }
        let abs = base.join(&e.rel);
        if std::fs::symlink_metadata(&abs).is_ok() {
            ffi::convert(&abs, &e.identity, e.in_sync, e.dir && !e.listed)?;
            continue;
        }
        let (Some(parent), Some(name)) = (abs.parent(), e.rel.file_name()) else {
            continue;
        };
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid Unicode", e.rel.display()),
            )
        })?;
        ffi::create_placeholders(
            parent,
            &[NewPlaceholder {
                name: name.to_owned(),
                identity: e.identity.clone(),
                meta: ffi::metadata(e.dir, e.size, e.mtime, e.read_only),
                dir: e.dir,
            }],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_that_is_not_a_sync_root_is_renamed_as_it_is() {
        let d = crate::win::testkit::tempdir();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::write(a.join("sub").join("f.txt"), b"f").unwrap();
        relocate(&a, &b, &[1; 16]).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(b.join("sub").join("f.txt")).unwrap(), b"f");
    }
}
