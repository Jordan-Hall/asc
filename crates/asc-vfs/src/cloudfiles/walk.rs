//! The on-disk walk of a Cloud Files root, and what is built on it: reconcile at start (SPEC
//! §10.2 item 7), the user's deletions (Cloud Files keeps no tombstones), and dehydration (§10.2
//! item 9).
//!
//! The walk reads only directories that are listed (populated): a directory placeholder that was
//! never listed has nothing of the user's below it, and reading it would list it. It never
//! descends into `.git`, a redirected name (§10.5) or a link, and it is bounded.
//!
//! * Reconcile journals a non-placeholder file, or a placeholder that is out of sync or carries
//!   another identity than its tree entry, as `modified` (tracked) or `created`; an untracked
//!   plain directory as `created`; and a tracked child missing from a listed directory as
//!   `deleted` (a directory expands to every tracked path below it). A directory moved before it
//!   was listed carries another tree than its path's entry: it is listed from its identity and
//!   walked.
//! * Dehydration takes a clean hydrated placeholder (in sync, all bytes on disk) back to empty
//!   with `CfUpdatePlaceholder(DEHYDRATE | MARK_IN_SYNC | VERIFY_IN_SYNC)`: the in-sync check makes
//!   a write that lands meanwhile fail the call instead of losing data, and a file open in
//!   another process fails the exclusive open.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::SystemTime;

use asc_core::object::Kind;
use asc_core::path::{Name, RepoPath, casefold};
use asc_wc::journal::Op;
use asc_wc::vstatus::MAX_WALK;
use windows::Win32::Storage::CloudFilters::{
    CF_UPDATE_FLAG_DEHYDRATE, CF_UPDATE_FLAG_MARK_IN_SYNC, CF_UPDATE_FLAG_VERIFY_IN_SYNC,
    CF_UPDATE_FLAGS,
};

use super::Ident;
use super::ffi::{self, Probe};
use super::populate::populate;
use super::provider::{CloudFilesProvider, Inner, is_hydrated, vfs_io};
use crate::core::{DehydrateOutcome, Node, Result, VfsError};
use crate::win::block;
use crate::win::paths::is_redirected;

pub use crate::win::Reconciled;
use crate::win::idle::{Candidate, select};
pub use crate::win::idle::{DEFAULT_BUDGET, IDLE, Policy, Report};

/// Most entries one on-disk directory may list.
pub const MAX_DIR_ENTRIES: usize = 1 << 20;
/// Most entries one walk visits, and most paths one directory deletion expands to.
pub const MAX_ENTRIES: usize = MAX_WALK;

fn too_many(what: &str) -> io::Error {
    io::Error::other(format!("{what} has more than {MAX_ENTRIES} entries"))
}

/// One on-disk entry met by the walk.
pub(crate) struct Found {
    pub path: RepoPath,
    pub abs: std::path::PathBuf,
    pub probe: Probe,
}

impl Found {
    /// A directory the walk may enter (not a link).
    pub fn is_dir(&self) -> bool {
        self.probe.is_dir() && !self.probe.is_foreign_reparse()
    }
}

pub(crate) trait Visitor {
    /// Called for every on-disk entry; `true` enters a directory. A directory that was never
    /// listed is entered only after the visitor lists it.
    fn entry(&mut self, f: &Found) -> io::Result<bool>;

    /// Called once per walked directory (`None` is the root) with the case-folded names on disk.
    fn dir_done(&mut self, _dir: Option<&RepoPath>, _names: &HashSet<Vec<u8>>) -> io::Result<()> {
        Ok(())
    }
}

/// Walks the listed directories at and below `start` (the root when `None`).
pub(crate) fn walk(inner: &Inner, start: Option<&RepoPath>, v: &mut dyn Visitor) -> io::Result<()> {
    let mut stack: Vec<Option<RepoPath>> = vec![start.cloned()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Some(abs) = inner.abs(dir.as_ref()) else {
            continue;
        };
        match ffi::probe_opt(&abs)? {
            Some(p) if p.is_dir() && !p.is_foreign_reparse() && !p.is_unpopulated() => {}
            _ => continue,
        }
        let rd = match std::fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(e) if ffi::is_not_found(&e) => continue,
            Err(e) => return Err(e),
        };
        let mut names = HashSet::new();
        for e in rd {
            let e = e?;
            if names.len() >= MAX_DIR_ENTRIES {
                return Err(too_many("a directory"));
            }
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err(too_many("the on-disk walk"));
            }
            let os = e.file_name();
            let Some(s) = os.to_str() else { continue };
            let Ok(name) = Name::new(s.as_bytes()) else {
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
            let Some(child) = inner.abs(Some(&path)) else {
                continue;
            };
            let Some(probe) = ffi::probe_opt(&child)? else {
                continue;
            };
            let found = Found {
                path,
                abs: child,
                probe,
            };
            if v.entry(&found)? && found.is_dir() {
                stack.push(Some(found.path));
            }
        }
        v.dir_done(dir.as_ref(), &names)?;
    }
    Ok(())
}

/// Whether the file at a probe still is the tree entry `tracked`: an in-sync placeholder with its
/// identity.
fn is_clean(p: &Probe, tracked: Option<&Node>) -> bool {
    p.is_placeholder()
        && !p.is_foreign_reparse()
        && p.in_sync
        && tracked.is_some_and(|n| p.ident() == Some(Ident::of(n)))
}

/// Lists a directory placeholder that was never listed, so the walk can enter it.
fn open_up(inner: &Inner, f: &Found) -> io::Result<()> {
    if f.probe.is_unpopulated() {
        populate(inner, Some(&f.path), &f.abs)?;
    }
    Ok(())
}

/// The tracked children of `dir` (base tree) not among `names`.
fn missing_in(
    inner: &Inner,
    dir: Option<&RepoPath>,
    names: &HashSet<Vec<u8>>,
) -> io::Result<Vec<Node>> {
    let listing = block(&inner.cfg.rt, inner.cfg.core.read_dir(dir)).map_err(vfs_io)?;
    Ok(listing
        .map(|l| {
            l.entries
                .into_iter()
                .filter(|n| !names.contains(&casefold(n.path.file_name().as_bytes())))
                .filter(|n| !crate::win::paths::dropped(&inner.cfg.redirections, &n.path))
                .collect()
        })
        .unwrap_or_default())
}

struct Reconcile<'a> {
    inner: &'a Inner,
    out: Reconciled,
}

impl Reconcile<'_> {
    fn record(&mut self, op: Op, path: &RepoPath) {
        self.inner.cfg.journal.record(op, path.as_bytes());
        match op {
            Op::Modified => self.out.modified += 1,
            Op::Created => self.out.created += 1,
            _ => self.out.deleted += 1,
        }
    }

    /// Every tracked path below the base-tree directory `dir`, as `deleted`.
    fn expand_deleted(&mut self, dir: &RepoPath) -> io::Result<()> {
        let mut stack = vec![dir.clone()];
        let mut n = 0usize;
        while let Some(d) = stack.pop() {
            let listing = block(&self.inner.cfg.rt, self.inner.cfg.core.read_dir(Some(&d)))
                .map_err(vfs_io)?;
            let Some(listing) = listing else { continue };
            for node in listing.entries {
                n += 1;
                if n > MAX_ENTRIES {
                    return Err(too_many("a deleted directory"));
                }
                if node.kind == Kind::Dir {
                    stack.push(node.path);
                } else {
                    self.record(Op::Deleted, &node.path);
                }
            }
        }
        Ok(())
    }
}

impl Visitor for Reconcile<'_> {
    fn entry(&mut self, f: &Found) -> io::Result<bool> {
        self.out.examined += 1;
        let tracked = self.inner.resolve(&f.path)?;
        if f.is_dir() {
            let tracked_dir = tracked.as_ref().filter(|n| n.kind == Kind::Dir);
            match tracked_dir {
                None => {
                    if tracked.is_some() {
                        // A directory where the tree has a file or gitlink: the file is gone.
                        self.record(Op::Deleted, &f.path);
                    }
                    self.record(Op::Created, &f.path);
                    open_up(self.inner, f)?;
                    Ok(true)
                }
                Some(n) => {
                    if f.probe.is_unpopulated() {
                        if f.probe.ident() == Some(Ident::of(n)) {
                            // Never listed and still the tree it was made from: nothing changed.
                            return Ok(false);
                        }
                        open_up(self.inner, f)?;
                    }
                    Ok(true)
                }
            }
        } else {
            if is_clean(&f.probe, tracked.as_ref()) {
                return Ok(false);
            }
            match tracked {
                Some(n) if n.kind == Kind::Dir => {
                    self.expand_deleted(&n.path)?;
                    self.record(Op::Created, &f.path);
                }
                Some(_) => self.record(Op::Modified, &f.path),
                None => self.record(Op::Created, &f.path),
            }
            Ok(false)
        }
    }

    fn dir_done(&mut self, dir: Option<&RepoPath>, names: &HashSet<Vec<u8>>) -> io::Result<()> {
        for node in missing_in(self.inner, dir, names)? {
            if node.kind == Kind::Dir {
                self.expand_deleted(&node.path)?;
            } else {
                self.record(Op::Deleted, &node.path);
            }
        }
        Ok(())
    }
}

/// Journals the difference between the disk and the base tree.
pub(crate) fn reconcile(inner: &Inner) -> io::Result<Reconciled> {
    let mut r = Reconcile {
        inner,
        out: Reconciled::default(),
    };
    walk(inner, None, &mut r)?;
    Ok(r.out)
}

/// Collects tracked children missing from listed directories.
struct Missing<'a> {
    inner: &'a Inner,
    out: Vec<RepoPath>,
}

impl Visitor for Missing<'_> {
    fn entry(&mut self, f: &Found) -> io::Result<bool> {
        Ok(f.is_dir())
    }

    fn dir_done(&mut self, dir: Option<&RepoPath>, names: &HashSet<Vec<u8>>) -> io::Result<()> {
        self.out.extend(
            missing_in(self.inner, dir, names)?
                .into_iter()
                .map(|n| n.path),
        );
        Ok(())
    }
}

/// The tracked paths missing from listed directories, sorted: the deletions the disk shows. A
/// deleted directory is its own path.
pub(crate) fn missing(inner: &Inner) -> io::Result<Vec<RepoPath>> {
    let mut m = Missing {
        inner,
        out: Vec::new(),
    };
    walk(inner, None, &mut m)?;
    m.out.sort();
    m.out.dedup();
    Ok(m.out)
}

/// The size a clean hydrated placeholder holds, or `None` when it is not one.
fn clean_hydrated_size(p: &Probe) -> Option<u64> {
    if p.is_dir() || p.is_foreign_reparse() || !p.is_placeholder() || !p.in_sync {
        return None;
    }
    match p.ident() {
        Some(Ident::File { size, .. }) if is_hydrated(p) => Some(size),
        _ => None,
    }
}

struct Scan<'a> {
    inner: &'a Inner,
    out: Vec<Candidate>,
}

impl Visitor for Scan<'_> {
    fn entry(&mut self, f: &Found) -> io::Result<bool> {
        if f.is_dir() {
            return Ok(true);
        }
        if let Some(size) = clean_hydrated_size(&f.probe) {
            let last_read = self
                .inner
                .cfg
                .core
                .stats()
                .last_read_of(&f.path)
                .unwrap_or(self.inner.started);
            self.out.push(Candidate {
                path: f.path.clone(),
                size,
                last_read,
            });
        }
        Ok(false)
    }
}

/// The clean hydrated files at and below `start`.
fn scan(inner: &Inner, start: Option<&RepoPath>) -> io::Result<Vec<Candidate>> {
    let mut s = Scan {
        inner,
        out: Vec::new(),
    };
    walk(inner, start, &mut s)?;
    Ok(s.out)
}

/// How one dehydration ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Back to an empty placeholder (or it already was one).
    Done,
    /// Not eligible, changed meanwhile, or in use.
    Skipped,
}

/// Dehydrates the file at `abs` if it is eligible right now.
pub(crate) fn dehydrate_one(abs: &Path) -> io::Result<Outcome> {
    let Some(p) = ffi::probe_opt(abs)? else {
        return Ok(Outcome::Skipped);
    };
    if p.is_dir() || p.is_foreign_reparse() || !p.is_placeholder() || !p.in_sync {
        return Ok(Outcome::Skipped);
    }
    if clean_hydrated_size(&p).is_none() {
        // An in-sync placeholder without its data is already what dehydration produces.
        return Ok(Outcome::Done);
    }
    let was_ro = ffi::clear_read_only(abs, p.attrs)?;
    let r = ffi::open_exclusive(abs).and_then(|h| {
        ffi::update(
            &h,
            None,
            None,
            CF_UPDATE_FLAGS(
                CF_UPDATE_FLAG_DEHYDRATE.0
                    | CF_UPDATE_FLAG_MARK_IN_SYNC.0
                    | CF_UPDATE_FLAG_VERIFY_IN_SYNC.0,
            ),
        )
    });
    if was_ro {
        let _ = ffi::set_attributes(abs, p.attrs);
    }
    Ok(if r.is_ok() {
        Outcome::Done
    } else {
        Outcome::Skipped
    })
}

impl CloudFilesProvider {
    /// Reconcile at start (SPEC §10.2 item 7): walks only listed directories, never enters
    /// redirected names, and appends `modified`, `created` and `deleted` hints to the journal. A
    /// deleted directory expands to every tracked path below it. The caller then resumes an
    /// interrupted switch with [`CloudFilesProvider::resume`].
    pub fn reconcile(&self) -> Result<Reconciled> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        Ok(reconcile(&inner)?)
    }

    /// The bytes of the clean hydrated placeholders in the workspace.
    pub fn hydrated_bytes(&self) -> Result<u64> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        Ok(scan(&inner, None)?.iter().map(|c| c.size).sum())
    }

    /// The idle pass at `now`. The daemon runs it after 60 s without notifications.
    pub fn dehydrate_idle(&self, policy: &Policy, now: SystemTime) -> Result<Report> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let chosen = select(scan(&inner, None)?, policy, now);
        let mut report = Report::default();
        for c in chosen {
            let Some(abs) = inner.abs(Some(&c.path)) else {
                report.skipped.push(c.path);
                continue;
            };
            match dehydrate_one(&abs)? {
                Outcome::Done => {
                    report.files += 1;
                    report.bytes += c.size;
                }
                Outcome::Skipped => report.skipped.push(c.path),
            }
        }
        Ok(report)
    }

    /// Dehydrates the clean hydrated files at and below `paths` (the whole workspace when empty).
    pub(crate) fn dehydrate_paths(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let mut out = DehydrateOutcome::default();
        let starts: Vec<Option<&RepoPath>> = if paths.is_empty() {
            vec![None]
        } else {
            paths.iter().map(Some).collect()
        };
        for start in starts {
            if let Some(p) = start {
                let abs = inner.abs(Some(p)).ok_or(VfsError::BadPath)?;
                match ffi::probe_opt(&abs)? {
                    Some(pr) if pr.is_dir() && !pr.is_foreign_reparse() => {}
                    Some(_) => {
                        match dehydrate_one(&abs)? {
                            Outcome::Done => out.dehydrated += 1,
                            Outcome::Skipped => out.skipped.push(p.clone()),
                        }
                        continue;
                    }
                    None => {
                        out.skipped.push(p.clone());
                        continue;
                    }
                }
            }
            for c in scan(&inner, start)? {
                let Some(abs) = inner.abs(Some(&c.path)) else {
                    out.skipped.push(c.path);
                    continue;
                };
                match dehydrate_one(&abs)? {
                    Outcome::Done => out.dehydrated += 1,
                    Outcome::Skipped => out.skipped.push(c.path),
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_probe(state: u32, attrs: u32, on_disk: u64, in_sync: bool, size: u64) -> Probe {
        Probe {
            attrs,
            state,
            on_disk,
            in_sync,
            identity: Ident::File {
                id: asc_core::id::FileId::from_bytes([1; 32]),
                size,
            }
            .encode(),
        }
    }

    const PLACEHOLDER: u32 = 0x1;
    const PARTIAL: u32 = 0x10;

    #[test]
    fn only_in_sync_full_placeholders_are_dehydrated() {
        assert_eq!(
            clean_hydrated_size(&file_probe(PLACEHOLDER, 0, 5, true, 5)),
            Some(5)
        );
        // Partial data, out of sync, not a placeholder, or a directory: never.
        assert_eq!(
            clean_hydrated_size(&file_probe(PLACEHOLDER | PARTIAL, 0, 0, true, 5)),
            None
        );
        assert_eq!(
            clean_hydrated_size(&file_probe(PLACEHOLDER, 0, 5, false, 5)),
            None
        );
        assert_eq!(clean_hydrated_size(&file_probe(0, 0, 5, true, 5)), None);
        assert_eq!(
            clean_hydrated_size(&file_probe(
                PLACEHOLDER,
                ffi::FILE_ATTRIBUTE_DIRECTORY,
                5,
                true,
                5
            )),
            None
        );
    }

    #[test]
    fn a_clean_file_is_an_in_sync_placeholder_of_its_entry() {
        let n = Node {
            path: RepoPath::new("a").unwrap(),
            kind: Kind::File,
            id: [1; 32],
            size: 5,
            git: asc_core::id::GitOid::from_bytes([0; 20]),
            read_only: false,
        };
        assert!(is_clean(&file_probe(PLACEHOLDER, 0, 0, true, 5), Some(&n)));
        assert!(!is_clean(
            &file_probe(PLACEHOLDER, 0, 0, false, 5),
            Some(&n)
        ));
        assert!(!is_clean(&file_probe(PLACEHOLDER, 0, 0, true, 6), Some(&n)));
        assert!(!is_clean(&file_probe(PLACEHOLDER, 0, 0, true, 5), None));
        assert!(!is_clean(&file_probe(0, 0, 5, true, 5), Some(&n)));
    }
}
