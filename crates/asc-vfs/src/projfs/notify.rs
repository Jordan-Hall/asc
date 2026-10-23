//! The notification callback (SPEC §10.2 item 4, §10.5): file changes append to the journal.
//! Any path with a redirected component or a `.git` component is dropped before any other work,
//! so build output in `node_modules` and friends never reaches the journal.

use std::ffi::c_void;

use asc_core::path::RepoPath;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;

use super::data::inner_of;
use super::ffi::{self, CallbackData, Hresult, Pcwstr, notify as n};
use super::{guard, rel_path};

/// The notifications the root mapping asks for.
pub const MOUNT_MASK: u32 = n::NEW_FILE_CREATED
    | n::FILE_OVERWRITTEN
    | n::CLOSED_FILE_MODIFIED
    | n::CLOSED_FILE_DELETED
    | n::FILE_RENAMED;

/// Whether one path component is a redirected name. Windows names are case-insensitive, so the
/// folded spelling is checked as well as the exact one.
pub fn is_redirected(red: &Redirections, name: &[u8]) -> bool {
    red.is_redirected_name(name) || red.is_redirected_name(&name.to_ascii_lowercase())
}

/// Whether a path is outside what the journal tracks: it has a redirected component or `.git`.
pub fn dropped(red: &Redirections, path: &RepoPath) -> bool {
    path.components()
        .any(|c| c.eq_ignore_ascii_case(b".git") || is_redirected(red, c))
}

/// The journal entries of one notification: `(op, path)` pairs, in order.
pub fn entries(
    red: &Redirections,
    note: u32,
    is_dir: bool,
    path: &RepoPath,
    dest: Option<&RepoPath>,
) -> Vec<(Op, RepoPath)> {
    let keep = |p: &RepoPath| !dropped(red, p);
    let mut out = Vec::new();
    match note {
        n::NEW_FILE_CREATED if keep(path) => out.push((Op::Created, path.clone())),
        n::FILE_OVERWRITTEN | n::CLOSED_FILE_MODIFIED if !is_dir && keep(path) => {
            out.push((Op::Modified, path.clone()));
        }
        n::CLOSED_FILE_DELETED if keep(path) => out.push((Op::Deleted, path.clone())),
        n::FILE_RENAMED => {
            // The source is recorded whatever the destination is; a rename out of the root or
            // into a redirected directory is a deletion of the source.
            let to = dest.filter(|d| keep(d));
            if keep(path) {
                let op = match (is_dir, to.is_some()) {
                    (true, true) => Op::DirRenamedFrom,
                    (false, true) => Op::RenamedFrom,
                    (_, false) => Op::Deleted,
                };
                out.push((op, path.clone()));
            }
            if let Some(to) = to {
                out.push((
                    if is_dir {
                        Op::DirRenamedTo
                    } else {
                        Op::Created
                    },
                    to.clone(),
                ));
            }
        }
        _ => {}
    }
    out
}

/// # Safety
/// Called by ProjFS with valid pointers; `dest` is null or NUL terminated.
pub(crate) unsafe extern "system" fn notification(
    d: *const CallbackData,
    is_dir: u8,
    note: u32,
    dest: Pcwstr,
    _params: *mut c_void,
) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes a valid pointer for the duration of the callback.
        let Some((sh, d)) = (unsafe { inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        // SAFETY: NUL terminated or null.
        let Some(w) = (unsafe { ffi::wide_to_string(d.file_path_name) }) else {
            return ffi::S_OK;
        };
        let Ok(Some(path)) = rel_path(&w) else {
            return ffi::S_OK;
        };
        let red = &sh.cfg.redirections;
        // Dropped before any other work: neither side of a rename is looked at otherwise.
        if note != n::FILE_RENAMED && dropped(red, &path) {
            return ffi::S_OK;
        }
        // SAFETY: NUL terminated or null.
        let to = unsafe { ffi::wide_to_string(dest) }
            .and_then(|w| rel_path(&w).ok())
            .flatten();
        for (op, p) in entries(red, note, is_dir != 0, &path, to.as_ref()) {
            sh.cfg.journal.record(op, p.as_bytes());
        }
        ffi::S_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn red() -> Redirections {
        Redirections::parse("node_modules\ndist\n")
    }

    #[test]
    fn redirected_paths_are_dropped() {
        let r = red();
        for p in [
            "node_modules/x",
            "a/node_modules/b/c",
            "Node_Modules/x",
            "dist",
        ] {
            assert!(dropped(&r, &rp(p)), "{p}");
            assert!(entries(&r, n::NEW_FILE_CREATED, false, &rp(p), None).is_empty());
        }
        assert!(!dropped(&r, &rp("src/dist.rs")));
        assert!(!dropped(&r, &rp("src/lib.rs")));
    }

    #[test]
    fn plain_changes_are_journaled() {
        let r = red();
        let p = rp("src/a.rs");
        assert_eq!(
            entries(&r, n::NEW_FILE_CREATED, false, &p, None),
            vec![(Op::Created, p.clone())]
        );
        assert_eq!(
            entries(&r, n::CLOSED_FILE_MODIFIED, false, &p, None),
            vec![(Op::Modified, p.clone())]
        );
        assert_eq!(
            entries(&r, n::FILE_OVERWRITTEN, false, &p, None),
            vec![(Op::Modified, p.clone())]
        );
        assert_eq!(
            entries(&r, n::CLOSED_FILE_DELETED, false, &p, None),
            vec![(Op::Deleted, p.clone())]
        );
        assert!(entries(&r, n::CLOSED_FILE_MODIFIED, true, &p, None).is_empty());
        assert!(entries(&r, n::FILE_OPENED, false, &p, None).is_empty());
    }

    #[test]
    fn renames_record_both_sides() {
        let r = red();
        let (a, b) = (rp("a.rs"), rp("b.rs"));
        assert_eq!(
            entries(&r, n::FILE_RENAMED, false, &a, Some(&b)),
            vec![(Op::RenamedFrom, a.clone()), (Op::Created, b.clone())]
        );
        assert_eq!(
            entries(&r, n::FILE_RENAMED, true, &a, Some(&b)),
            vec![
                (Op::DirRenamedFrom, a.clone()),
                (Op::DirRenamedTo, b.clone())
            ]
        );
        // Out of the root, or into a redirected directory: the source is gone.
        assert_eq!(
            entries(&r, n::FILE_RENAMED, false, &a, None),
            vec![(Op::Deleted, a.clone())]
        );
        assert_eq!(
            entries(
                &r,
                n::FILE_RENAMED,
                false,
                &a,
                Some(&rp("node_modules/a.rs"))
            ),
            vec![(Op::Deleted, a.clone())]
        );
        // Out of a redirected directory: only the new file.
        assert_eq!(
            entries(&r, n::FILE_RENAMED, false, &rp("dist/a.rs"), Some(&b)),
            vec![(Op::Created, b)]
        );
    }
}
