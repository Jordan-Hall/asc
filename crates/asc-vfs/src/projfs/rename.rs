//! Directory-rename pre-hydration (SPEC §10.2 item 6, [audit: projfs-dir-tombstone-reconcile]).
//!
//! On `PRE_RENAME` of a directory the whole subtree under the source is hydrated, so every
//! descendant is a real file before the rename proceeds. The work runs on worker threads that are
//! not callback threads, and the callback returns only after they finish. ProjFS is started with a
//! pool of [`pool_threads`] callback threads; at most [`workers`] of them can be waited on by
//! hydration reads, plus the one thread parked in the `PRE_RENAME` callback itself (renames are
//! serialised), so at least [`FREE_THREADS`] stay free to answer the reads. If hydration fails
//! (offline, missing object) the rename is denied with `STATUS_ACCESS_DENIED` and the log names the
//! object.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use asc_core::path::{Name, RepoPath};

use super::ffi::{self, Hresult, file_state as fs};
use super::notify::is_redirected;
use super::provider::{Inner, lock};
use super::reconcile::state_bits;

/// Callback threads that stay free while a rename hydrates.
pub const FREE_THREADS: usize = 2;
/// Most hydration worker threads.
pub const MAX_WORKERS: usize = 8;
/// Most files one rename hydrates; a larger directory is refused rather than stalling the shell.
pub const MAX_FILES: usize = 2_000_000;
/// Most directories one rename scans.
pub const MAX_DIRS: usize = 1_000_000;
/// `HRESULT_FROM_NT(STATUS_ACCESS_DENIED)`.
pub const STATUS_ACCESS_DENIED: Hresult = 0xD000_0022_u32 as i32;

/// Callback threads ProjFS is started with.
pub fn pool_threads() -> u32 {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    u32::try_from((cores * 2).clamp(8, 64)).unwrap_or(8)
}

/// Hydration worker threads for a pool of `pool` callback threads: the pool minus the free
/// threads and the callback's own.
pub fn workers(pool: u32) -> usize {
    (pool as usize)
        .saturating_sub(FREE_THREADS + 1)
        .clamp(1, MAX_WORKERS)
}

/// What a pre-hydration did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hydrated {
    pub files: u64,
    pub bytes: u64,
}

/// Why a pre-hydration failed; `path` is the file that could not be read.
#[derive(Debug)]
pub struct Failure {
    pub path: RepoPath,
    pub error: io::Error,
}

/// Whether the file at `bits` still needs its data fetched.
fn needs_data(bits: Option<u32>) -> bool {
    match bits {
        None => true,
        Some(b) => {
            b & (fs::FULL | fs::DIRTY_PLACEHOLDER | fs::HYDRATED_PLACEHOLDER | fs::TOMBSTONE) == 0
        }
    }
}

/// Reads a whole file to hydrate it. Nothing is kept.
fn read_through(path: &std::path::Path) -> io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
    }
}

/// Hydrates every file below `src`. Redirected names, `.git` and links are left alone.
pub(crate) fn hydrate_subtree(inner: &Inner, src: &RepoPath) -> Result<Hydrated, Failure> {
    let first_failure: Mutex<Option<Failure>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let totals = Mutex::new(Hydrated::default());
    let (tx, rx) = mpsc::sync_channel::<(RepoPath, PathBuf)>(1024);
    let rx = Mutex::new(rx);
    let nworkers = workers(pool_threads());
    let fail = |path: RepoPath, error: io::Error| {
        let mut slot = lock(&first_failure);
        if slot.is_none() {
            *slot = Some(Failure { path, error });
        }
        stop.store(true, Ordering::SeqCst);
    };
    std::thread::scope(|s| {
        for _ in 0..nworkers {
            s.spawn(|| {
                loop {
                    let job = lock(&rx).recv();
                    let Ok((path, abs)) = job else { return };
                    if stop.load(Ordering::SeqCst) {
                        continue;
                    }
                    match read_through(&abs) {
                        Ok(n) => {
                            let mut t = lock(&totals);
                            t.files += 1;
                            t.bytes += n;
                        }
                        Err(e) => fail(path, e),
                    }
                }
            });
        }
        // The scanner lists directories (which enumerates them through the provider) and queues
        // every file that still needs data.
        let mut stack = vec![src.clone()];
        let (mut dirs, mut files) = (0usize, 0usize);
        'scan: while let Some(dir) = stack.pop() {
            dirs += 1;
            if dirs > MAX_DIRS {
                fail(dir, io::Error::other("too many directories to hydrate"));
                break;
            }
            let Some(abs) = inner.safe.resolve(&dir) else {
                continue;
            };
            let rd = match std::fs::read_dir(&abs) {
                Ok(rd) => rd,
                Err(e) => {
                    fail(dir, e);
                    break;
                }
            };
            for e in rd {
                if stop.load(Ordering::SeqCst) {
                    break 'scan;
                }
                let e = match e {
                    Ok(e) => e,
                    Err(err) => {
                        fail(dir.clone(), err);
                        break 'scan;
                    }
                };
                let Some(name) = e.file_name().to_str().and_then(|n| Name::new(n).ok()) else {
                    continue;
                };
                if name.as_bytes().eq_ignore_ascii_case(b".git")
                    || is_redirected(&inner.cfg.redirections, name.as_bytes())
                {
                    continue;
                }
                let path = dir.join(&name);
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                let m = match e.metadata() {
                    Ok(m) => m,
                    Err(err) => {
                        fail(path, err);
                        break 'scan;
                    }
                };
                if is_reparse(&m) {
                    continue;
                }
                if m.is_dir() {
                    stack.push(path);
                    continue;
                }
                files += 1;
                if files > MAX_FILES {
                    fail(path, io::Error::other("too many files to hydrate"));
                    break 'scan;
                }
                let bits = match state_bits(inner, &path) {
                    Ok(b) => b,
                    Err(err) => {
                        fail(path, err);
                        break 'scan;
                    }
                };
                if needs_data(bits) && tx.send((path, e.path())).is_err() {
                    break 'scan;
                }
            }
        }
        drop(tx);
    });
    match first_failure
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
    {
        Some(f) => Err(f),
        None => Ok(totals.into_inner().unwrap_or_else(|e| e.into_inner())),
    }
}

fn is_reparse(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & ffi::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// The `PRE_RENAME` callback body for the (non-redirected) `path`. Files need nothing: ProjFS
/// itself hydrates a renamed placeholder file. A directory is hydrated first.
pub(crate) fn pre_rename(inner: &Inner, path: &RepoPath, is_dir: bool) -> Hresult {
    if !is_dir {
        return ffi::S_OK;
    }
    // One directory rename at a time, so waiting readers can never use up the callback pool.
    let _one = lock(&inner.rename_gate);
    if inner.stopping.load(Ordering::SeqCst) {
        return STATUS_ACCESS_DENIED;
    }
    match hydrate_subtree(inner, path) {
        Ok(_) => ffi::S_OK,
        Err(f) => {
            // The daemon log names the missing object.
            let id = super::block(&inner.cfg.rt, inner.cfg.core.resolve(&f.path))
                .ok()
                .flatten()
                .map(|n| format!("{:02x?}", &n.id[..8]));
            eprintln!(
                "asc-vfs: rename of {} denied: cannot hydrate {} (object {}): {}",
                String::from_utf8_lossy(path.as_bytes()),
                String::from_utf8_lossy(f.path.as_bytes()),
                id.as_deref().unwrap_or("unknown"),
                f.error
            );
            STATUS_ACCESS_DENIED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_leave_the_free_threads() {
        for pool in [4u32, 8, 16, 64] {
            let w = workers(pool);
            assert!((1..=MAX_WORKERS).contains(&w));
            if pool as usize > FREE_THREADS + 2 {
                assert!(w + FREE_THREADS < pool as usize, "pool {pool} workers {w}");
            }
        }
        let p = pool_threads();
        assert!((8..=64).contains(&p));
        assert!(workers(p) + 1 + FREE_THREADS <= p as usize);
    }

    #[test]
    fn needs_data_only_for_empty_placeholders() {
        assert!(needs_data(None));
        assert!(needs_data(Some(fs::PLACEHOLDER)));
        assert!(!needs_data(Some(
            fs::HYDRATED_PLACEHOLDER | fs::PLACEHOLDER
        )));
        assert!(!needs_data(Some(fs::FULL)));
        assert!(!needs_data(Some(fs::DIRTY_PLACEHOLDER)));
    }

    #[test]
    fn access_denied_is_the_nt_status() {
        assert_eq!(STATUS_ACCESS_DENIED as u32, 0xD000_0022);
    }
}

#[cfg(test)]
mod lab_tests {
    use super::*;
    use crate::projfs::switch::lab::Lab;
    use asc_wc::journal::Op;

    const TREE: &[(&str, &[u8])] = &[
        ("pkg/a.txt", b"aaaa"),
        ("pkg/inner/b.txt", b"bbbbbb"),
        ("pkg/inner/c.txt", b"c"),
        ("other.txt", b"other"),
    ];

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn offline_denied() {
        // One object is not in any store and cannot be fetched: the move is refused whole.
        let Some(lab) = Lab::offline(TREE, &["pkg/inner/b.txt"]) else {
            return;
        };
        let err = std::fs::rename(lab.at("pkg"), lab.at("moved")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(lab.at("pkg").exists());
        assert!(!lab.at("moved").exists());
        // Files and directories that need no fetch are unaffected.
        assert_eq!(std::fs::read(lab.at("other.txt")).unwrap(), b"other");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn hydrates_the_subtree_then_the_rename_goes_through() {
        let Some(lab) = Lab::new(TREE) else { return };
        std::fs::rename(lab.at("pkg"), lab.at("moved")).unwrap();
        // Every descendant is a real file with the right bytes.
        assert_eq!(std::fs::read(lab.at("moved/a.txt")).unwrap(), b"aaaa");
        assert_eq!(
            std::fs::read(lab.at("moved/inner/b.txt")).unwrap(),
            b"bbbbbb"
        );
        assert_eq!(std::fs::read(lab.at("moved/inner/c.txt")).unwrap(), b"c");
        assert!(!lab.at("pkg").exists());
        // Exactly the three files were fetched, by the pre-hydration.
        assert_eq!(lab.core.stats().snapshot().total_hydrations, 3);
        // The journal learns of the move.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < until
            && !(lab.journal.has(Op::DirRenamedFrom, "pkg")
                && lab.journal.has(Op::DirRenamedTo, "moved"))
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            lab.journal.has(Op::DirRenamedFrom, "pkg"),
            "{:?}",
            lab.journal.dump()
        );
        assert!(
            lab.journal.has(Op::DirRenamedTo, "moved"),
            "{:?}",
            lab.journal.dump()
        );
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn renaming_a_file_needs_no_pre_hydration() {
        let Some(lab) = Lab::new(TREE) else { return };
        std::fs::rename(lab.at("other.txt"), lab.at("renamed.txt")).unwrap();
        assert_eq!(std::fs::read(lab.at("renamed.txt")).unwrap(), b"other");
    }
}
