//! Directory enumeration callbacks (SPEC §10.2 item 4): sessions keyed by GUID, answered from the
//! base tree, filtered with `PrjFileNameMatch` and ordered with `PrjFileNameCompare`. Enumeration
//! never hydrates: it lists tree metadata only.

use asc_core::object::Kind;
use asc_core::path::RepoPath;

use super::ffi::{self, CallbackData, DirBuf, FileBasicInfo, Guid, Hresult, Pcwstr};
use super::provider::{Inner, lock};
use super::{block, guard, rel_path};
use crate::core::Node;

/// Most entries one directory listing holds; a larger directory is refused, not truncated.
pub const MAX_DIR_ENTRIES: usize = 1 << 20;
/// Most enumeration sessions open at once.
pub const MAX_SESSIONS: usize = 1 << 16;

struct Entry {
    /// NUL terminated.
    name: Vec<u16>,
    info: FileBasicInfo,
}

/// One open enumeration.
pub(crate) struct Session {
    entries: Vec<Entry>,
    cursor: usize,
    /// `None` until the first `get_dir` of a scan captured the search expression.
    search: Option<Option<Vec<u16>>>,
}

/// Placeholder metadata of a node at time `t` (Unix seconds).
pub(crate) fn basic_info(node: &Node, t: i64) -> FileBasicInfo {
    let dir = node.is_dir() || node.kind == Kind::Gitlink;
    let ft = ffi::filetime(t);
    let mut attrs = if dir {
        ffi::FILE_ATTRIBUTE_DIRECTORY
    } else {
        ffi::FILE_ATTRIBUTE_NORMAL
    };
    if node.read_only && !dir {
        attrs = ffi::FILE_ATTRIBUTE_READONLY;
    }
    FileBasicInfo {
        is_directory: u8::from(dir),
        file_size: if dir {
            0
        } else {
            i64::try_from(node.size).unwrap_or(i64::MAX)
        },
        creation_time: ft,
        last_access_time: ft,
        last_write_time: ft,
        change_time: ft,
        file_attributes: attrs,
    }
}

fn build(sh: &Inner, dir: Option<&RepoPath>) -> Result<Vec<Entry>, Hresult> {
    let listing = block(&sh.cfg.rt, sh.cfg.core.read_dir(dir))
        .map_err(|_| ffi::ERROR_IO_DEVICE)?
        .ok_or(ffi::ERROR_PATH_NOT_FOUND)?;
    if listing.entries.len() > MAX_DIR_ENTRIES {
        return Err(ffi::ERROR_OUTOFMEMORY);
    }
    let paths: Vec<RepoPath> = listing.entries.iter().map(|n| n.path.clone()).collect();
    let times = sh
        .cfg
        .times
        .times(&paths)
        .map_err(|_| ffi::ERROR_IO_DEVICE)?;
    if times.len() != paths.len() {
        return Err(ffi::ERROR_IO_DEVICE);
    }
    let mut out = Vec::with_capacity(paths.len());
    for (node, t) in listing.entries.iter().zip(times) {
        // A name the OS cannot hold as UTF-16 is not projected.
        let fname = node.path.file_name();
        let Ok(name) = std::str::from_utf8(fname.as_bytes()) else {
            continue;
        };
        out.push(Entry {
            name: ffi::to_wide(name),
            info: basic_info(node, t),
        });
    }
    out.sort_by(|a, b| {
        // SAFETY: both names are NUL terminated.
        let c = unsafe { (sh.api.name_compare)(a.name.as_ptr(), b.name.as_ptr()) };
        c.cmp(&0)
    });
    Ok(out)
}

/// # Safety
/// Called by ProjFS with a valid callback data and enumeration id.
pub(crate) unsafe extern "system" fn start_dir(d: *const CallbackData, id: *const Guid) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes valid pointers for the duration of the callback.
        let Some((sh, d)) = (unsafe { super::data::inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        // SAFETY: as above.
        let id = unsafe { *id };
        // SAFETY: `file_path_name` is NUL terminated or null.
        let Some(w) = (unsafe { ffi::wide_to_string(d.file_path_name) }) else {
            return ffi::ERROR_INVALID_PARAMETER;
        };
        let Ok(dir) = rel_path(&w) else {
            return ffi::ERROR_PATH_NOT_FOUND;
        };
        if sh.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return ffi::ERROR_OPERATION_ABORTED;
        }
        if lock(&sh.sessions).len() >= MAX_SESSIONS {
            return ffi::ERROR_OUTOFMEMORY;
        }
        // The tree is read with no lock held.
        let entries = match build(sh, dir.as_ref()) {
            Ok(e) => e,
            Err(hr) => return hr,
        };
        lock(&sh.sessions).insert(
            id,
            Session {
                entries,
                cursor: 0,
                search: None,
            },
        );
        ffi::S_OK
    })
}

/// # Safety
/// Called by ProjFS with a valid callback data and enumeration id.
pub(crate) unsafe extern "system" fn end_dir(d: *const CallbackData, id: *const Guid) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes valid pointers for the duration of the callback.
        let Some((sh, _)) = (unsafe { super::data::inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        // SAFETY: as above.
        let id = unsafe { *id };
        lock(&sh.sessions).remove(&id);
        ffi::S_OK
    })
}

/// # Safety
/// Called by ProjFS with valid pointers; `search` is null or NUL terminated.
pub(crate) unsafe extern "system" fn get_dir(
    d: *const CallbackData,
    id: *const Guid,
    search: Pcwstr,
    buf: DirBuf,
) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes valid pointers for the duration of the callback.
        let Some((sh, d)) = (unsafe { super::data::inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        // SAFETY: as above.
        let id = unsafe { *id };
        // The session is taken out of the table so no lock is held across ProjFS calls.
        let Some(mut s) = lock(&sh.sessions).remove(&id) else {
            return ffi::ERROR_INVALID_PARAMETER;
        };
        if d.flags & ffi::CB_ENUM_RESTART_SCAN != 0 {
            s.cursor = 0;
            s.search = None;
        }
        if s.search.is_none() {
            s.search = Some(if search.is_null() {
                None
            } else {
                // SAFETY: NUL terminated per the contract; bounded copy.
                unsafe { bounded_wide(search) }
            });
        }
        let hr = fill(
            sh,
            &mut s,
            d.flags & ffi::CB_ENUM_RETURN_SINGLE_ENTRY != 0,
            buf,
        );
        lock(&sh.sessions).insert(id, s);
        hr
    })
}

/// A bounded copy (with the NUL) of a wide string, or `None` when it is unterminated.
unsafe fn bounded_wide(p: Pcwstr) -> Option<Vec<u16>> {
    let mut v = Vec::new();
    for i in 0..ffi::MAX_WIDE {
        // SAFETY: reads up to the terminator, at most MAX_WIDE units.
        let c = unsafe { *p.add(i) };
        v.push(c);
        if c == 0 {
            return Some(v);
        }
    }
    None
}

fn fill(sh: &Inner, s: &mut Session, single: bool, buf: DirBuf) -> Hresult {
    let pattern = s.search.as_ref().and_then(Option::as_ref);
    let mut added = 0usize;
    while let Some(e) = s.entries.get(s.cursor) {
        let hit = pattern.is_none_or(|p| {
            // SAFETY: both strings are NUL terminated.
            unsafe { (sh.api.name_match)(e.name.as_ptr(), p.as_ptr()) != 0 }
        });
        if hit {
            // SAFETY: valid name, info and buffer handle.
            let hr = unsafe { (sh.api.fill_dir_entry)(e.name.as_ptr(), &e.info, buf) };
            if hr == ffi::ERROR_INSUFFICIENT_BUFFER {
                // Full: what fit is returned now, and this entry leads the next call.
                return if added > 0 { ffi::S_OK } else { hr };
            }
            if hr != ffi::S_OK {
                return hr;
            }
            added += 1;
        }
        s.cursor += 1;
        if single && added > 0 {
            break;
        }
    }
    ffi::S_OK
}

#[cfg(test)]
mod tests {
    use asc_core::id::GitOid;

    use super::*;

    fn node(kind: Kind, size: u64, ro: bool) -> Node {
        Node {
            path: RepoPath::new("a").unwrap(),
            kind,
            id: [1; 32],
            size,
            git: GitOid::from_bytes([0; 20]),
            read_only: ro,
        }
    }

    #[test]
    fn basic_info_for_files_and_dirs() {
        let f = basic_info(&node(Kind::File, 7, false), 100);
        assert_eq!((f.is_directory, f.file_size), (0, 7));
        assert_eq!(f.last_write_time, ffi::filetime(100));
        assert_eq!(f.creation_time, f.change_time);
        let d = basic_info(&node(Kind::Dir, 9, false), 100);
        assert_eq!((d.is_directory, d.file_size), (1, 0));
        assert_eq!(d.file_attributes, ffi::FILE_ATTRIBUTE_DIRECTORY);
        let g = basic_info(&node(Kind::Gitlink, 0, false), 100);
        assert_eq!(g.is_directory, 1);
        let r = basic_info(&node(Kind::File, 1, true), 100);
        assert_eq!(r.file_attributes, ffi::FILE_ATTRIBUTE_READONLY);
    }
}
