//! Dehydration (SPEC §10.2 item 9): clean hydrated placeholders go back to empty ones with
//! `PrjDeleteFile(PRJ_UPDATE_ALLOW_READ_ONLY)`. Only the state `HydratedPlaceholder` (no dirty
//! data or metadata, not a tombstone, not a full file) is eligible, and a file that any process
//! has open is left alone because the delete then fails. No dirty flag is ever passed.
//!
//! The idle pass ([`ProjfsProvider::dehydrate_idle`]) takes a file when no process has read it for
//! [`IDLE`], and then the least recently hydrated files until the hydrated bytes are within the
//! budget. The hydration log lives in memory, so a file the log has never seen counts as read when
//! this provider started: after a daemon restart nothing is dropped for the idle period.

use std::io;
use std::time::{Duration, SystemTime};

use asc_core::path::RepoPath;

use super::ffi::{self, update};
use super::provider::{Inner, ProjfsProvider};
use super::reconcile::{Found, Visitor, is_clean_hydrated, rel_wide, state_bits, walk};
use crate::core::{DehydrateOutcome, Result, VfsError};

/// No process has read the file for this long: it is dropped.
pub const IDLE: Duration = Duration::from_secs(3 * 24 * 3600);
/// `asc.vfs.hydrationBudget` default: 500 MiB.
pub const DEFAULT_BUDGET: u64 = 500 * 1024 * 1024;

/// When the idle pass drops files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub idle: Duration,
    pub budget: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            idle: IDLE,
            budget: DEFAULT_BUDGET,
        }
    }
}

/// What an idle pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub files: u64,
    /// Bytes of the files that went back to empty.
    pub bytes: u64,
    /// Candidates that could not be dehydrated (open, changed meanwhile).
    pub skipped: Vec<RepoPath>,
}

/// How one dehydration ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Back to an empty placeholder (or it already was one).
    Done,
    /// Not eligible or in use.
    Skipped,
}

/// A hydrated clean file, collected by the scan.
struct Candidate {
    path: RepoPath,
    size: u64,
    last_read: SystemTime,
}

struct Scan<'a> {
    inner: &'a Inner,
    out: Vec<Candidate>,
}

impl Visitor for Scan<'_> {
    fn entry(&mut self, f: &Found) -> io::Result<bool> {
        if f.dir {
            return Ok(!f.reparse);
        }
        if !f.reparse && f.bits.is_some_and(is_clean_hydrated) {
            let last_read = self
                .inner
                .cfg
                .core
                .stats()
                .last_read_of(&f.path)
                .unwrap_or(self.inner.started);
            self.out.push(Candidate {
                path: f.path.clone(),
                size: f.size,
                last_read,
            });
        }
        Ok(false)
    }
}

/// The clean hydrated files below `start`.
fn scan(inner: &Inner, start: Option<&RepoPath>) -> io::Result<Vec<Candidate>> {
    let mut s = Scan {
        inner,
        out: Vec::new(),
    };
    walk(inner, start, &mut s)?;
    Ok(s.out)
}

/// Dehydrates the file at `path` if it is eligible right now.
pub(crate) fn dehydrate_one(inner: &Inner, path: &RepoPath) -> io::Result<Outcome> {
    let Some(bits) = state_bits(inner, path)? else {
        return Ok(Outcome::Skipped);
    };
    if bits & ffi::file_state::HYDRATED_PLACEHOLDER == 0 {
        // An empty placeholder is already what dehydration produces.
        let empty = bits & ffi::file_state::PLACEHOLDER != 0
            && !super::reconcile::is_dirty(bits)
            && !super::reconcile::is_tombstone(bits);
        return Ok(if empty {
            Outcome::Done
        } else {
            Outcome::Skipped
        });
    }
    if !is_clean_hydrated(bits) {
        return Ok(Outcome::Skipped);
    }
    let Some(rel) = rel_wide(path) else {
        return Ok(Outcome::Skipped);
    };
    let (hr, _cause) = inner.api.delete(
        inner.ctx.load(std::sync::atomic::Ordering::SeqCst),
        &rel,
        update::ALLOW_READ_ONLY,
    );
    Ok(if hr == ffi::S_OK {
        Outcome::Done
    } else {
        Outcome::Skipped
    })
}

impl ProjfsProvider {
    /// The bytes of the clean hydrated placeholders in the workspace.
    pub fn hydrated_bytes(&self) -> Result<u64> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        Ok(scan(&inner, None)?.iter().map(|c| c.size).sum())
    }

    /// The idle pass at `now`. The daemon runs it after 60 s without notifications.
    pub fn dehydrate_idle(&self, policy: &Policy, now: SystemTime) -> Result<Report> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let mut all = scan(&inner, None)?;
        let total: u64 = all.iter().map(|c| c.size).sum();
        let idle_for =
            |c: &Candidate| now.duration_since(c.last_read).unwrap_or_default() >= policy.idle;
        let (mut chosen, mut rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|c| idle_for(c));
        let mut left = total.saturating_sub(chosen.iter().map(|c| c.size).sum());
        if left > policy.budget {
            // Least recently hydrated first, until the rest fits.
            rest.sort_by(|a, b| a.last_read.cmp(&b.last_read).then(a.path.cmp(&b.path)));
            for c in rest {
                if left <= policy.budget {
                    break;
                }
                left = left.saturating_sub(c.size);
                chosen.push(c);
            }
        }
        let mut report = Report::default();
        for c in chosen {
            match dehydrate_one(&inner, &c.path)? {
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
                // A file named directly.
                match std::fs::symlink_metadata(inner.safe.resolve(p).ok_or(VfsError::BadPath)?) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => {
                        match dehydrate_one(&inner, p)? {
                            Outcome::Done => out.dehydrated += 1,
                            Outcome::Skipped => out.skipped.push(p.clone()),
                        }
                        continue;
                    }
                    Err(_) => {
                        out.skipped.push(p.clone());
                        continue;
                    }
                }
            }
            for c in scan(&inner, start)? {
                match dehydrate_one(&inner, &c.path)? {
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
    use crate::core::Provider;
    use crate::projfs::switch::lab::{Holder, Lab, p};
    use crate::projfs::switch::tests::V1;

    fn total(files: &[(&str, &[u8])]) -> u64 {
        files.iter().map(|(_, d)| d.len() as u64).sum()
    }

    fn eager() -> Policy {
        Policy {
            idle: Duration::ZERO,
            budget: u64::MAX,
        }
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn returns_hydrated_bytes_to_zero_for_clean_files() {
        let Some(lab) = Lab::new(V1) else { return };
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
        lab.hydrate(V1);
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), total(V1));
        let r = lab
            .provider
            .dehydrate_idle(&eager(), SystemTime::now())
            .unwrap();
        assert_eq!(r.files, V1.len() as u64, "{r:?}");
        assert_eq!(r.bytes, total(V1));
        assert!(r.skipped.is_empty(), "{r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
        // The data is fetched again on the next read.
        let before = lab.core.stats().snapshot().total_hydrations;
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"alpha");
        assert_eq!(lab.core.stats().snapshot().total_hydrations, before + 1);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn never_touches_dirty_files() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        std::fs::write(lab.at("a.txt"), b"my edit").unwrap();
        std::fs::write(lab.at("mine.txt"), b"untracked").unwrap();
        std::fs::remove_file(lab.at("b.txt")).unwrap();
        let r = lab
            .provider
            .dehydrate_idle(&eager(), SystemTime::now())
            .unwrap();
        assert_eq!(r.files, (V1.len() - 2) as u64, "{r:?}");
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"my edit");
        assert_eq!(std::fs::read(lab.at("mine.txt")).unwrap(), b"untracked");
        assert!(!lab.at("b.txt").exists());
        // Asking for the dirty file by name changes nothing either.
        let o = lab.provider.dehydrate(&[p("a.txt")]).unwrap();
        assert_eq!(
            (o.dehydrated, o.skipped.as_slice()),
            (0, [p("a.txt")].as_slice())
        );
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"my edit");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn skips_recently_read() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let idle = Policy {
            idle: IDLE,
            budget: u64::MAX,
        };
        let now = SystemTime::now();
        let r = lab.provider.dehydrate_idle(&idle, now).unwrap();
        assert_eq!(r.files, 0, "read just now: {r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), total(V1));
        // Four days later nothing has been read for three.
        let later = now + Duration::from_secs(4 * 24 * 3600);
        let r = lab.provider.dehydrate_idle(&idle, later).unwrap();
        assert_eq!(r.files, V1.len() as u64, "{r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn the_budget_drops_the_least_recently_hydrated_first() {
        let files: &[(&str, &[u8])] = &[
            ("one.txt", b"1111111111"),
            ("two.txt", b"2222222222"),
            ("three.txt", b"3333333333"),
        ];
        let Some(lab) = Lab::new(files) else { return };
        for (path, data) in files {
            assert_eq!(&std::fs::read(lab.at(path)).unwrap(), data);
            std::thread::sleep(Duration::from_millis(20));
        }
        let policy = Policy {
            idle: IDLE,
            budget: 10,
        };
        let r = lab
            .provider
            .dehydrate_idle(&policy, SystemTime::now())
            .unwrap();
        assert_eq!(r.files, 2, "{r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 10);
        // The newest is the one left hydrated.
        let before = lab.core.stats().snapshot().total_hydrations;
        let _ = std::fs::read(lab.at("three.txt")).unwrap();
        assert_eq!(lab.core.stats().snapshot().total_hydrations, before);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn a_file_open_in_a_process_is_skipped() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let holder = Holder::new(&lab.at("a.txt"));
        let r = lab
            .provider
            .dehydrate_idle(&eager(), SystemTime::now())
            .unwrap();
        assert_eq!(r.skipped, [p("a.txt")], "{r:?}");
        assert_eq!(r.files, (V1.len() - 1) as u64);
        holder.release();
        let r = lab
            .provider
            .dehydrate_idle(&eager(), SystemTime::now())
            .unwrap();
        assert_eq!(r.files, 1, "{r:?}");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn a_directory_pathspec_covers_what_is_below() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let o = lab.provider.dehydrate(&[p("sub")]).unwrap();
        assert_eq!(o.dehydrated, 1, "{o:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), total(V1) - 4);
        let all = lab.provider.dehydrate(&[]).unwrap();
        assert_eq!(all.dehydrated, V1.len() - 1, "{all:?}");
    }
}
