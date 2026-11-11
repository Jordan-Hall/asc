//! The change watcher: Cloud Files tells a provider about deletions and renames of placeholders
//! only, so the journal is fed by `ReadDirectoryChangesW` over the whole root. The path rules
//! apply as in ProjFS: a redirected or `.git` path never reaches the journal, a rename out of the
//! root (or into a redirected directory) is a deletion, and the deletions are remembered for
//! `worktree convert --plain`. An in-sync placeholder whose identity is the tracked entry is the
//! filter's own work (a listing or a hydration) and is not a change. An overflow marks the journal
//! lost, so the next reader reconciles.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use asc_core::path::RepoPath;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;
use windows::Win32::Foundation::{ERROR_OPERATION_ABORTED, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

use super::Ident;
use super::ffi::{self, Handle};
use super::provider::{Inner, lock};
use crate::win::paths::{dropped, track};
use crate::win::rel_path;

/// `FILE_ACTION_*`.
pub const ADDED: u32 = 1;
pub const REMOVED: u32 = 2;
pub const MODIFIED: u32 = 3;
pub const RENAMED_OLD: u32 = 4;
pub const RENAMED_NEW: u32 = 5;

/// Bytes of the change buffer (the most a network-free watch takes in one read).
const BUFFER: usize = 64 * 1024;
/// How often the thread looks at the stop flag, in milliseconds.
const POLL_MS: u32 = 200;

/// Parses a `FILE_NOTIFY_INFORMATION` chain into `(action, root-relative name)` pairs.
pub fn parse(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let u32_at = |i: usize| -> Option<u32> {
        buf.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    while let (Some(next), Some(action), Some(len)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8)) {
        let start = at + 12;
        let Some(name) = buf.get(start..start + len as usize) else {
            break;
        };
        let units: Vec<u16> = name
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        out.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    out
}

/// The journal entries of a batch of changes, in order. `is_dir` says whether a path is a
/// directory now; `noise` whether it is the filter's own work.
pub fn entries(
    red: &Redirections,
    batch: &[(u32, Option<RepoPath>)],
    is_dir: &dyn Fn(&RepoPath) -> bool,
    noise: &dyn Fn(&RepoPath) -> bool,
) -> Vec<(Op, RepoPath)> {
    let keep = |p: &RepoPath| !dropped(red, p);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < batch.len() {
        let (action, path) = &batch[i];
        i += 1;
        match *action {
            ADDED => {
                if let Some(p) = path
                    && keep(p)
                    && !noise(p)
                {
                    out.push((Op::Created, p.clone()));
                }
            }
            REMOVED => {
                if let Some(p) = path
                    && keep(p)
                {
                    out.push((Op::Deleted, p.clone()));
                }
            }
            MODIFIED => {
                if let Some(p) = path
                    && keep(p)
                    && !is_dir(p)
                    && !noise(p)
                {
                    out.push((Op::Modified, p.clone()));
                }
            }
            RENAMED_OLD => {
                // The new name follows; a missing one (or one in a dropped place) is a move away.
                let to = match batch.get(i) {
                    Some((RENAMED_NEW, to)) => {
                        i += 1;
                        to.clone().filter(|t| keep(t))
                    }
                    _ => None,
                };
                let dir = to.as_ref().is_some_and(is_dir);
                if let Some(p) = path
                    && keep(p)
                {
                    let op = match (dir, to.is_some()) {
                        (true, true) => Op::DirRenamedFrom,
                        (false, true) => Op::RenamedFrom,
                        (_, false) => Op::Deleted,
                    };
                    out.push((op, p.clone()));
                }
                if let Some(to) = to {
                    out.push((if dir { Op::DirRenamedTo } else { Op::Created }, to));
                }
            }
            RENAMED_NEW => {
                // Moved in from outside the watched tree.
                if let Some(p) = path
                    && keep(p)
                {
                    out.push((
                        if is_dir(p) {
                            Op::DirRenamedTo
                        } else {
                            Op::Created
                        },
                        p.clone(),
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether `path` is an in-sync placeholder carrying the identity of its tracked entry.
fn is_noise(inner: &Inner, path: &RepoPath) -> bool {
    let Some(abs) = inner.abs(Some(path)) else {
        return false;
    };
    let Ok(Some(p)) = ffi::probe_opt(&abs) else {
        return false;
    };
    if !p.is_placeholder() || !p.in_sync {
        return false;
    }
    match inner.resolve(path) {
        Ok(Some(n)) => p.ident() == Some(Ident::of(&n)),
        _ => false,
    }
}

fn is_dir_now(inner: &Inner, path: &RepoPath) -> bool {
    inner
        .abs(Some(path))
        .and_then(|a| std::fs::symlink_metadata(a).ok())
        .is_some_and(|m| m.is_dir())
}

/// Records one read's changes.
fn deliver(inner: &Inner, raw: &[(u32, String)]) {
    let batch: Vec<(u32, Option<RepoPath>)> = raw
        .iter()
        .map(|(a, n)| (*a, rel_path(n).ok().flatten()))
        .collect();
    let list = entries(
        &inner.cfg.redirections,
        &batch,
        &|p| is_dir_now(inner, p),
        &|p| is_noise(inner, p),
    );
    if list.is_empty() {
        return;
    }
    let mut deleted = lock(&inner.deleted);
    for (op, p) in list {
        inner.cfg.journal.record(op, p.as_bytes());
        track(&mut deleted, op, &p);
    }
}

/// The running watcher thread.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Opens the root and starts watching it.
    pub fn start(inner: Arc<Inner>) -> io::Result<Self> {
        let dir = open_dir(inner.safe.root())?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("asc-cf-watch".into())
            .spawn(move || run(&inner, &dir, &flag))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Stops the thread and waits for it.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_dir(p: &Path) -> io::Result<Handle> {
    let w = ffi::wide(p);
    // SAFETY: a NUL terminated path; the handle is owned by the result.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            ffi::SHARE_ALL,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|e| ffi::io_err(&e))?;
    Ok(Handle(h))
}

const FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(
    FILE_NOTIFY_CHANGE_FILE_NAME.0
        | FILE_NOTIFY_CHANGE_DIR_NAME.0
        | FILE_NOTIFY_CHANGE_SIZE.0
        | FILE_NOTIFY_CHANGE_LAST_WRITE.0,
);

fn run(inner: &Inner, dir: &Handle, stop: &AtomicBool) {
    // SAFETY: a manual-reset, unnamed event; owned below.
    let Ok(ev) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else {
        inner.cfg.journal.lost();
        return;
    };
    let event = Handle(ev);
    // `u64` keeps the buffer `DWORD` aligned.
    let mut buf = vec![0u64; BUFFER / 8];
    while !stop.load(Ordering::SeqCst) {
        let mut ov = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        // SAFETY: the buffer and the `OVERLAPPED` outlive the read: it completes or is cancelled
        // and waited for before either is touched again.
        let started = unsafe {
            ReadDirectoryChangesW(
                dir.0,
                buf.as_mut_ptr().cast(),
                BUFFER as u32,
                true,
                FILTER,
                None,
                Some(&raw mut ov),
                None,
            )
        };
        if started.is_err() {
            inner.cfg.journal.lost();
            return;
        }
        let mut n = 0u32;
        let done = loop {
            // SAFETY: the read above is pending on `ov`.
            match unsafe { GetOverlappedResultEx(dir.0, &ov, &mut n, POLL_MS, false) } {
                Ok(()) => break true,
                Err(e) if e.code() == windows::core::HRESULT::from_win32(WAIT_TIMEOUT.0) => {
                    if stop.load(Ordering::SeqCst) {
                        cancel(dir.0, &ov);
                        return;
                    }
                }
                Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => return,
                Err(_) => break false,
            }
        };
        if !done || n == 0 {
            // Too many changes for the buffer (or the read failed): hints were lost.
            inner.cfg.journal.lost();
            continue;
        }
        // SAFETY: the read wrote `n` bytes into the buffer.
        let bytes = unsafe {
            std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), (n as usize).min(BUFFER))
        };
        deliver(inner, &parse(bytes));
    }
}

/// Cancels the read pending on `ov` and waits for it to finish.
fn cancel(h: HANDLE, ov: &OVERLAPPED) {
    let mut n = 0u32;
    // SAFETY: `ov` is the pending read on `h`; waiting makes sure the kernel no longer uses it.
    unsafe {
        let _ = CancelIoEx(h, Some(ov));
        let _ = GetOverlappedResult(h, ov, &mut n, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend_from_slice(&next.to_le_bytes());
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn notify_records_parse_in_order() {
        let first = record(0, RENAMED_OLD, "src\\a.rs");
        let mut buf = record(first.len() as u32, RENAMED_OLD, "src\\a.rs");
        buf.extend(record(0, RENAMED_NEW, "src\\b.rs"));
        assert_eq!(
            parse(&buf),
            [
                (RENAMED_OLD, "src\\a.rs".to_string()),
                (RENAMED_NEW, "src\\b.rs".to_string())
            ]
        );
        assert!(parse(&[1, 2]).is_empty());
        // A length past the end stops the parse.
        let mut bad = record(0, ADDED, "x");
        bad[8] = 200;
        assert!(parse(&bad).is_empty());
    }

    #[test]
    fn changes_map_like_projfs_notifications() {
        let red = Redirections::parse("node_modules\n");
        let dirs = |p: &RepoPath| p.as_bytes() == b"newdir" || p.as_bytes() == b"moved";
        let quiet = |p: &RepoPath| p.as_bytes() == b"listed.txt";
        let batch = vec![
            (ADDED, Some(rp("new.txt"))),
            (ADDED, Some(rp("listed.txt"))),
            (MODIFIED, Some(rp("a.txt"))),
            (MODIFIED, Some(rp("newdir"))),
            (REMOVED, Some(rp("gone.txt"))),
            (RENAMED_OLD, Some(rp("b.txt"))),
            (RENAMED_NEW, Some(rp("c.txt"))),
            (RENAMED_OLD, Some(rp("dir"))),
            (RENAMED_NEW, Some(rp("moved"))),
            (RENAMED_OLD, Some(rp("d.txt"))),
            (RENAMED_NEW, Some(rp("node_modules/d.txt"))),
            (RENAMED_OLD, Some(rp("away.txt"))),
            (ADDED, Some(rp("node_modules/x.js"))),
            (ADDED, None),
            (RENAMED_NEW, Some(rp("newdir"))),
        ];
        let got = entries(&red, &batch, &dirs, &quiet);
        let want = vec![
            (Op::Created, rp("new.txt")),
            (Op::Modified, rp("a.txt")),
            (Op::Deleted, rp("gone.txt")),
            (Op::RenamedFrom, rp("b.txt")),
            (Op::Created, rp("c.txt")),
            (Op::DirRenamedFrom, rp("dir")),
            (Op::DirRenamedTo, rp("moved")),
            (Op::Deleted, rp("d.txt")),
            (Op::Deleted, rp("away.txt")),
            (Op::DirRenamedTo, rp("newdir")),
        ];
        assert_eq!(got, want);
    }
}
