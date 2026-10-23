//! Reconcile at start (SPEC §10.2 item 7) and the on-disk walk it shares with dehydration.
//!
//! The walk lists only what is physically on disk (`FIND_FIRST_EX_ON_DISK_ENTRIES_ONLY`), so a
//! workspace that has materialised little is cheap to walk and no virtual directory is enumerated
//! or hydrated. It never descends into redirected names (§10.5), `.git`, or a reparse point (a
//! nested worktree junction, a symlink), and it is bounded.

use std::collections::HashSet;
use std::io;

use asc_core::object::Kind;
use asc_core::path::{Name, RepoPath, casefold};
use asc_wc::journal::Op;
use asc_wc::vstatus::MAX_WALK;

use super::block;
use super::ffi::{self, file_state as fs};
use super::notify::is_redirected;
use super::provider::{Inner, ProjfsProvider};
use crate::core::{Result, VfsError};

/// Most entries one on-disk directory may list.
pub const MAX_DIR_ENTRIES: usize = 1 << 20;
/// Most entries one walk visits, and most paths one directory-tombstone expansion yields.
pub const MAX_ENTRIES: usize = MAX_WALK;

/// What reconcile found, for the daemon's metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// On-disk entries examined.
    pub examined: u64,
    pub modified: u64,
    pub created: u64,
    pub deleted: u64,
}

/// The root-relative, backslash separated, NUL terminated form of a path.
pub(crate) fn rel_wide(path: &RepoPath) -> Option<Vec<u16>> {
    let s = std::str::from_utf8(path.as_bytes()).ok()?;
    Some(ffi::to_wide(&s.replace('/', "\\")))
}

fn too_many(what: &str) -> io::Error {
    io::Error::other(format!("too many entries: {what}"))
}

/// The `PRJ_FILE_STATE` bits of `path`, or `None` when nothing of it is on disk (virtual, absent,
/// or outside the work tree because an ancestor is a link).
pub(crate) fn state_bits(inner: &Inner, path: &RepoPath) -> io::Result<Option<u32>> {
    let Some(abs) = inner.safe.resolve(path) else {
        return Ok(None);
    };
    match inner.api.file_state(&ffi::path_to_wide(&abs)) {
        Ok(bits) => Ok(Some(bits)),
        Err(hr) if hr == ffi::ERROR_FILE_NOT_FOUND || hr == ffi::ERROR_PATH_NOT_FOUND => Ok(None),
        Err(hr) => Err(io::Error::other(format!(
            "PrjGetOnDiskFileState: HRESULT {:#010x}",
            hr as u32
        ))),
    }
}

pub(crate) fn is_tombstone(bits: u32) -> bool {
    bits & fs::TOMBSTONE != 0
}

/// A file or directory whose data or metadata differs from the projection.
pub(crate) fn is_dirty(bits: u32) -> bool {
    bits & (fs::FULL | fs::DIRTY_PLACEHOLDER) != 0
}

/// A placeholder whose data was fetched and not changed.
pub(crate) fn is_clean_hydrated(bits: u32) -> bool {
    bits & fs::HYDRATED_PLACEHOLDER != 0
        && bits & (fs::FULL | fs::DIRTY_PLACEHOLDER | fs::TOMBSTONE) == 0
}

/// One on-disk entry met by the walk.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub path: RepoPath,
    pub dir: bool,
    pub reparse: bool,
    pub size: u64,
    /// `None` when the state could not be read as on disk (it vanished during the walk).
    pub bits: Option<u32>,
}

pub(crate) trait Visitor {
    /// Called for every on-disk entry; `true` descends into a directory.
    fn entry(&mut self, f: &Found) -> io::Result<bool>;

    /// Called once per walked directory (`None` is the root) with the case-folded names of its
    /// on-disk entries, tombstones included.
    fn dir_done(&mut self, _dir: Option<&RepoPath>, _names: &HashSet<Vec<u8>>) -> io::Result<()> {
        Ok(())
    }
}

/// Walks the on-disk entries below `start` (the root when `None`).
pub(crate) fn walk(inner: &Inner, start: Option<&RepoPath>, v: &mut dyn Visitor) -> io::Result<()> {
    let mut stack: Vec<Option<RepoPath>> = vec![start.cloned()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let abs = match &dir {
            None => inner.root.clone(),
            Some(d) => match inner.safe.resolve(d) {
                Some(a) => a,
                None => continue,
            },
        };
        let (list, more) = match ffi::read_dir_on_disk(&ffi::path_to_wide(&abs), MAX_DIR_ENTRIES) {
            Ok(x) => x,
            Err(e) if matches!(e.raw_os_error(), Some(2 | 3)) => continue,
            Err(e) => return Err(e),
        };
        if more {
            return Err(too_many("a directory"));
        }
        let mut names = HashSet::new();
        for e in list {
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err(too_many("the on-disk walk"));
            }
            let Ok(name) = Name::new(e.name.as_bytes()) else {
                continue;
            };
            names.insert(casefold(name.as_bytes()));
            if name.as_bytes().eq_ignore_ascii_case(b".git")
                || is_redirected(&inner.cfg.redirections, name.as_bytes())
            {
                continue;
            }
            let path = match &dir {
                None => match RepoPath::new(name.as_bytes().to_vec()) {
                    Ok(p) => p,
                    Err(_) => continue,
                },
                Some(d) => d.join(&name),
            };
            let found = Found {
                bits: state_bits(inner, &path)?,
                dir: e.is_dir() && !e.is_reparse(),
                reparse: e.is_reparse(),
                size: e.size,
                path: path.clone(),
            };
            if v.entry(&found)? && found.dir {
                stack.push(Some(path));
            }
        }
        v.dir_done(dir.as_ref(), &names)?;
    }
    Ok(())
}

/// Every tracked path below the base-tree directory `dir`, as `Deleted` hints. Metadata only.
fn expand_deleted(inner: &Inner, dir: &RepoPath, out: &mut Reconciled) -> io::Result<()> {
    let mut stack = vec![dir.clone()];
    let mut n = 0usize;
    while let Some(d) = stack.pop() {
        let listing = block(&inner.cfg.rt, inner.cfg.core.read_dir(Some(&d)))
            .map_err(|e| io::Error::other(e.to_string()))?;
        let Some(listing) = listing else { continue };
        for node in listing.entries {
            n += 1;
            if n > MAX_ENTRIES {
                return Err(too_many("a directory tombstone"));
            }
            if node.kind == Kind::Dir {
                stack.push(node.path);
            } else {
                inner.cfg.journal.record(Op::Deleted, node.path.as_bytes());
                out.deleted += 1;
            }
        }
    }
    Ok(())
}

struct Reconcile<'a> {
    inner: &'a Inner,
    out: Reconciled,
}

impl Reconcile<'_> {
    fn tracked(&self, path: &RepoPath) -> io::Result<Option<crate::core::Node>> {
        block(&self.inner.cfg.rt, self.inner.cfg.core.resolve(path))
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn deleted(&mut self, path: &RepoPath, dir: bool) -> io::Result<()> {
        if dir {
            // A directory tombstone is a deletion of every tracked path below it.
            expand_deleted(self.inner, path, &mut self.out)
        } else {
            self.inner.cfg.journal.record(Op::Deleted, path.as_bytes());
            self.out.deleted += 1;
            Ok(())
        }
    }
}

impl Visitor for Reconcile<'_> {
    fn entry(&mut self, f: &Found) -> io::Result<bool> {
        self.out.examined += 1;
        let Some(bits) = f.bits else {
            return Ok(false);
        };
        if is_tombstone(bits) {
            // The tree decides whether this was a directory.
            let dir = self.tracked(&f.path)?.is_some_and(|n| n.kind == Kind::Dir);
            self.deleted(&f.path, dir)?;
            return Ok(false);
        }
        let tracked = if is_dirty(bits) || f.dir {
            self.tracked(&f.path)?
        } else {
            None
        };
        if f.dir {
            if tracked.is_none() && bits & fs::FULL != 0 {
                self.inner
                    .cfg
                    .journal
                    .record(Op::Created, f.path.as_bytes());
                self.out.created += 1;
            }
            // Never descend through a link.
            return Ok(!f.reparse);
        }
        if is_dirty(bits) {
            if tracked.is_some_and(|n| n.kind != Kind::Dir) {
                self.inner
                    .cfg
                    .journal
                    .record(Op::Modified, f.path.as_bytes());
                self.out.modified += 1;
            } else {
                self.inner
                    .cfg
                    .journal
                    .record(Op::Created, f.path.as_bytes());
                self.out.created += 1;
            }
        }
        Ok(false)
    }

    /// A tracked child that is not among the on-disk names may still be a tombstone.
    fn dir_done(&mut self, dir: Option<&RepoPath>, names: &HashSet<Vec<u8>>) -> io::Result<()> {
        let listing = block(&self.inner.cfg.rt, self.inner.cfg.core.read_dir(dir))
            .map_err(|e| io::Error::other(e.to_string()))?;
        let Some(listing) = listing else {
            return Ok(());
        };
        for node in listing.entries {
            if names.contains(&casefold(node.path.file_name().as_bytes())) {
                continue;
            }
            if state_bits(self.inner, &node.path)?.is_some_and(is_tombstone) {
                self.deleted(&node.path, node.kind == Kind::Dir)?;
            }
        }
        Ok(())
    }
}

/// Journals the difference between the disk and the base tree.
pub(crate) fn run(inner: &Inner) -> io::Result<Reconciled> {
    let mut r = Reconcile {
        inner,
        out: Reconciled::default(),
    };
    walk(inner, None, &mut r)?;
    Ok(r.out)
}

impl ProjfsProvider {
    /// Reconcile at start (SPEC §10.2 item 7): enumerates only on-disk entries, never descends
    /// into redirected names, and appends `modified`, `created` and `deleted` hints to the
    /// journal. A directory tombstone expands to every tracked path below it. The caller then
    /// resumes an interrupted switch with [`ProjfsProvider::resume`].
    pub fn reconcile(&self) -> Result<Reconciled> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        Ok(run(&inner)?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::core::Provider;
    use crate::projfs::switch::lab::Lab;
    use crate::projfs::switch::tests::V1;

    /// Notifications arrive asynchronously; after they settle the journal is emptied, so what the
    /// test sees afterwards came from reconcile alone (the daemon was down).
    fn settle(lab: &Lab) {
        std::thread::sleep(Duration::from_millis(700));
        lab.journal.clear();
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn dir_tombstone() {
        let files: &[(&str, &[u8])] = &[
            ("src/a.rs", b"a"),
            ("src/b.rs", b"b"),
            ("src/deep/c.rs", b"c"),
            ("top.txt", b"top"),
        ];
        let Some(lab) = Lab::new(files) else { return };
        lab.hydrate(files);
        std::fs::remove_dir_all(lab.at("src")).unwrap();
        assert!(!lab.at("src").exists());
        settle(&lab);
        let r = lab.provider.reconcile().unwrap();
        let seen = lab.journal.dump();
        for p in ["src/a.rs", "src/b.rs", "src/deep/c.rs"] {
            assert!(lab.journal.has(Op::Deleted, p), "{p} in {seen:?}");
        }
        assert!(!lab.journal.has(Op::Deleted, "top.txt"), "{seen:?}");
        assert!(r.deleted >= 3, "{r:?}");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn file_changes_are_found() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        std::fs::write(lab.at("a.txt"), b"changed").unwrap();
        std::fs::remove_file(lab.at("c.txt")).unwrap();
        std::fs::write(lab.at("new.txt"), b"new").unwrap();
        settle(&lab);
        let r = lab.provider.reconcile().unwrap();
        let seen = lab.journal.dump();
        assert!(lab.journal.has(Op::Modified, "a.txt"), "{seen:?}");
        assert!(lab.journal.has(Op::Deleted, "c.txt"), "{seen:?}");
        assert!(lab.journal.has(Op::Created, "new.txt"), "{seen:?}");
        assert!(!lab.journal.has(Op::Modified, "b.txt"), "{seen:?}");
        assert_eq!((r.modified, r.deleted, r.created), (1, 1, 1), "{r:?}");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn skips_redirections() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let nm = lab.at("node_modules/pkg");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("index.js"), b"x").unwrap();
        std::fs::create_dir_all(lab.at("sub/node_modules")).unwrap();
        std::fs::write(lab.at("sub/node_modules/y.js"), b"y").unwrap();
        std::fs::write(lab.at("kept.txt"), b"y").unwrap();
        settle(&lab);
        let r = lab.provider.reconcile().unwrap();
        let seen = lab.journal.dump();
        assert!(lab.journal.has(Op::Created, "kept.txt"), "{seen:?}");
        assert!(!lab.journal.any_under("node_modules"), "{seen:?}");
        assert!(!lab.journal.any_under("sub/node_modules"), "{seen:?}");
        assert_eq!(r.created, 1, "{r:?}");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn a_fresh_workspace_walks_nothing() {
        let Some(lab) = Lab::new(V1) else { return };
        let r = lab.provider.reconcile().unwrap();
        assert_eq!(r, Reconciled::default());
        assert!(lab.journal.dump().is_empty());
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn an_unmounted_provider_refuses() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.provider.unmount().unwrap();
        assert!(matches!(
            lab.provider.reconcile(),
            Err(VfsError::NotMounted)
        ));
    }
}
