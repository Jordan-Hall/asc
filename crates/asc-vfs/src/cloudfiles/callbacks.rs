//! The callbacks the filter calls (SPEC §10.2b): listings (`FETCH_PLACEHOLDERS`), file data from
//! the identity each placeholder carries (`FETCH_DATA`, never HEAD) with the hydration policy
//! run first, cancels, and the delete and rename checks that keep a locked path where it is.
//!
//! A callback that is not answered fails the operation, so every path through a callback ends in
//! exactly one `CfExecute` (or a set of transfers that covers the request).

use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use asc_core::path::RepoPath;
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Storage::CloudFilters::*;
use windows::core::PCWSTR;

use super::ffi::{self, STATUS_ACCESS_DENIED, STATUS_CLOUD_FILE_ACCESS_DENIED, STATUS_SUCCESS};
use super::populate::{listing_for, placeholders};
use super::provider::{Inner, lock};
use super::{Ident, guard};
use crate::core::ContentRef;
use crate::policy::Decision;
use crate::win::block;

/// Bytes per `TRANSFER_DATA`.
pub const TRANSFER_BUFFER: usize = 1 << 20;
/// The filter's data alignment.
const ALIGN: u64 = 4096;

/// Connects the callbacks to the sync root `root`; `ctx` is the `Arc::into_raw` of the state.
pub(crate) fn connect(root: &Path, ctx: *const c_void) -> io::Result<CF_CONNECTION_KEY> {
    let table = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(cancel_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(notify_rename),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let w = ffi::plain_wide(root);
    // SAFETY: a NUL terminated path, a table ending in `NONE` (the filter copies it), and a
    // context that stays valid until the disconnect.
    unsafe {
        CfConnectSyncRoot(
            PCWSTR(w.as_ptr()),
            table.as_ptr(),
            Some(ctx),
            CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        )
    }
    .map_err(|e| ffi::io_err(&e))
}

/// The state of a callback.
///
/// # Safety
/// `info` is null or the pointer the filter passed, valid for the callback.
unsafe fn inner_of<'a>(info: *const CF_CALLBACK_INFO) -> Option<(&'a Inner, &'a CF_CALLBACK_INFO)> {
    // SAFETY: per the contract.
    let info = unsafe { info.as_ref() }?;
    // SAFETY: the context is the `Arc::into_raw` pointer given to `CfConnectSyncRoot`, alive
    // until after `CfDisconnectSyncRoot` returns.
    let sh = unsafe { info.CallbackContext.cast::<Inner>().cast_const().as_ref() }?;
    Some((sh, info))
}

/// An owning handle on the state for asynchronous work.
///
/// # Safety
/// As [`inner_of`].
unsafe fn arc_of(info: &CF_CALLBACK_INFO) -> Option<Arc<Inner>> {
    let p = info.CallbackContext.cast::<Inner>().cast_const();
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` came from `Arc::into_raw` and the count is raised before it is adopted.
    unsafe {
        Arc::increment_strong_count(p);
        Some(Arc::from_raw(p))
    }
}

/// The root-relative path of the callback's file; `None` outside the root or invalid.
fn path_of(sh: &Inner, info: &CF_CALLBACK_INFO) -> Option<Option<RepoPath>> {
    // SAFETY: NUL terminated or null, valid for the callback.
    let s = unsafe { ffi::from_wide(info.NormalizedPath) }?;
    sh.repo_path(&s)
}

/// The image of the process that caused the callback.
fn image_of(info: &CF_CALLBACK_INFO) -> String {
    // SAFETY: the process info is null or valid for the callback, as is its image path.
    unsafe {
        info.ProcessInfo
            .as_ref()
            .and_then(|p| ffi::from_wide(p.ImagePath))
            .unwrap_or_default()
    }
}

/// Whether this process caused the callback (the switch and dehydration move locked paths too).
fn is_own(info: &CF_CALLBACK_INFO) -> bool {
    // SAFETY: the process info is null or valid for the callback.
    unsafe { info.ProcessInfo.as_ref() }.is_some_and(|p| p.ProcessId == std::process::id())
}

/// The identity bytes of the callback's placeholder.
fn identity_of(info: &CF_CALLBACK_INFO) -> &[u8] {
    if info.FileIdentity.is_null() || info.FileIdentityLength == 0 {
        return &[];
    }
    // SAFETY: the filter passes `FileIdentityLength` readable bytes, valid for the callback.
    unsafe {
        std::slice::from_raw_parts(
            info.FileIdentity.cast::<u8>(),
            info.FileIdentityLength as usize,
        )
    }
}

/// The keys of one request, owned so the answer can be sent after the callback returned.
#[derive(Clone, Copy)]
struct Keys {
    connection: CF_CONNECTION_KEY,
    transfer: i64,
    request: i64,
}

impl Keys {
    fn of(info: &CF_CALLBACK_INFO) -> Self {
        Self {
            connection: info.ConnectionKey,
            transfer: info.TransferKey,
            request: info.RequestKey,
        }
    }

    fn op(&self, ty: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
        CF_OPERATION_INFO {
            StructSize: size_of::<CF_OPERATION_INFO>() as u32,
            Type: ty,
            ConnectionKey: self.connection,
            TransferKey: self.transfer,
            RequestKey: self.request,
            ..Default::default()
        }
    }
}

fn params() -> CF_OPERATION_PARAMETERS {
    CF_OPERATION_PARAMETERS {
        ParamSize: size_of::<CF_OPERATION_PARAMETERS>() as u32,
        ..Default::default()
    }
}

/// Sends a listing; `items` empty with a failure status refuses it.
fn transfer_placeholders(
    k: Keys,
    items: &[ffi::NewPlaceholder],
    status: NTSTATUS,
) -> windows::core::Result<()> {
    let mut names = Vec::new();
    let mut infos = ffi::create_infos(items, &mut names);
    let mut prm = params();
    prm.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_4 {
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CompletionStatus: status,
        PlaceholderTotalCount: infos.len() as i64,
        PlaceholderArray: if infos.is_empty() {
            std::ptr::null_mut()
        } else {
            infos.as_mut_ptr()
        },
        PlaceholderCount: infos.len() as u32,
        EntriesProcessed: 0,
    };
    let oi = k.op(CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    // SAFETY: the operation and parameters are complete; `infos` points into `names` and
    // `items`, alive for the call.
    unsafe { CfExecute(&oi, &mut prm) }
}

/// Sends `buf` at `offset`, or fails the range `offset..offset+len` with `status`.
fn transfer_data(
    k: Keys,
    buf: Option<&[u8]>,
    offset: u64,
    len: u64,
    status: NTSTATUS,
) -> windows::core::Result<()> {
    let mut prm = params();
    prm.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: status,
        Buffer: buf.map_or(std::ptr::null(), |b| b.as_ptr().cast()),
        Offset: i64::try_from(offset).unwrap_or(i64::MAX),
        Length: i64::try_from(len).unwrap_or(i64::MAX),
    };
    let oi = k.op(CF_OPERATION_TYPE_TRANSFER_DATA);
    // SAFETY: the buffer is null or holds `len` bytes, alive for the call.
    unsafe { CfExecute(&oi, &mut prm) }
}

/// Answers a delete or rename; a refusal the filter does not take as `STATUS_ACCESS_DENIED` is
/// sent as the Cloud Files access-denied status.
fn ack(k: Keys, rename: bool, allow: bool) {
    let send = |status: NTSTATUS| {
        let mut prm = params();
        let oi = if rename {
            prm.Anonymous.AckRename = CF_OPERATION_PARAMETERS_0_6 {
                Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                CompletionStatus: status,
            };
            k.op(CF_OPERATION_TYPE_ACK_RENAME)
        } else {
            prm.Anonymous.AckDelete = CF_OPERATION_PARAMETERS_0_7 {
                Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                CompletionStatus: status,
            };
            k.op(CF_OPERATION_TYPE_ACK_DELETE)
        };
        // SAFETY: complete operation and parameters.
        unsafe { CfExecute(&oi, &mut prm) }
    };
    if allow {
        let _ = send(STATUS_SUCCESS);
    } else if send(STATUS_ACCESS_DENIED).is_err() {
        let _ = send(STATUS_CLOUD_FILE_ACCESS_DENIED);
    }
}

/// # Safety
/// Called by the filter with valid callback info and parameters.
unsafe extern "system" fn fetch_placeholders(
    info: *const CF_CALLBACK_INFO,
    _p: *const CF_CALLBACK_PARAMETERS,
) {
    guard(|| {
        // SAFETY: the filter passes a valid pointer for the duration of the callback.
        let Some((sh, info)) = (unsafe { inner_of(info) }) else {
            return;
        };
        let k = Keys::of(info);
        let refuse = || {
            let _ = transfer_placeholders(k, &[], ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
        };
        let Some(path) = path_of(sh, info) else {
            return refuse();
        };
        let ident = if path.is_none() {
            Some(Ident::Root)
        } else {
            Ident::decode(identity_of(info))
        };
        let listing = match block(&sh.cfg.rt, listing_for(&sh.cfg.core, path.as_ref(), ident)) {
            Ok(l) => l,
            Err(_) => return refuse(),
        };
        let items = match placeholders(sh, &listing) {
            Ok(v) => v,
            Err(_) => return refuse(),
        };
        if transfer_placeholders(k, &items, STATUS_SUCCESS).is_err() {
            refuse();
        }
    });
}

/// One `FETCH_DATA` request, owned so it can outlive the callback.
struct Request {
    keys: Keys,
    path: RepoPath,
    content: ContentRef,
    /// Aligned start of the range to send; the range runs to the end of the file (full
    /// hydration).
    offset: u64,
    image: String,
    cancel: Arc<AtomicBool>,
}

/// # Safety
/// Called by the filter with valid callback info and parameters.
unsafe extern "system" fn fetch_data(
    info: *const CF_CALLBACK_INFO,
    p: *const CF_CALLBACK_PARAMETERS,
) {
    guard(|| {
        // SAFETY: the filter passes valid pointers for the duration of the callback.
        let Some((sh, cb)) = (unsafe { inner_of(info) }) else {
            return;
        };
        // SAFETY: as above.
        let Some(p) = (unsafe { p.as_ref() }) else {
            return;
        };
        // SAFETY: the parameters of a `FETCH_DATA` callback.
        let fd = unsafe { p.Anonymous.FetchData };
        let k = Keys::of(cb);
        let off = u64::try_from(fd.RequiredFileOffset).unwrap_or(0);
        let len = u64::try_from(fd.RequiredLength).unwrap_or(0);
        let fail = |status: NTSTATUS| {
            let _ = transfer_data(k, None, off, len, status);
        };
        // The placeholder says which content it is; HEAD is never consulted.
        let (Some(Some(path)), Some(Ident::File { id, size })) =
            (path_of(sh, cb), Ident::decode(identity_of(cb)))
        else {
            return fail(ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
        };
        let image = image_of(cb);
        // The hydration policy runs before any fetch (SPEC §10.2 item 5).
        if sh.cfg.core.policy().check(&image) == Decision::Deny {
            sh.cfg.core.stats().record_denied(&image);
            return fail(STATUS_CLOUD_FILE_ACCESS_DENIED);
        }
        let start = off - off % ALIGN;
        if start >= size {
            return fail(ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
        }
        let rq = Request {
            keys: k,
            path,
            content: ContentRef { id, size },
            offset: start,
            image,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if sh.is_local(id) {
            block(&sh.cfg.rt, serve(sh, &rq));
            return;
        }
        // SAFETY: the callback info is valid and belongs to this connection.
        let Some(arc) = (unsafe { arc_of(cb) }) else {
            return fail(ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
        };
        let Some(live) = arc.begin() else {
            return fail(ffi::STATUS_CLOUD_FILE_REQUEST_CANCELED);
        };
        lock(&arc.transfers).insert(k.transfer, rq.cancel.clone());
        let task = arc.clone();
        arc.cfg.rt.spawn(async move {
            let _live = live;
            serve(&task, &rq).await;
            lock(&task.transfers).remove(&rq.keys.transfer);
        });
    });
}

/// Sends the content from `rq.offset` to the end of the file, in aligned buffers. On an error or
/// a cancel the rest of the range is failed, so the request always completes. Verification is
/// done by the content stream (every inline file and chunk is checked against its id).
async fn serve(sh: &Inner, rq: &Request) {
    let size = rq.content.size;
    let mut at = rq.offset;
    let fail_rest = |at: u64, status: NTSTATUS| {
        if at < size {
            let _ = transfer_data(rq.keys, None, at, size - at, status);
        }
    };
    if rq.offset == 0 {
        sh.cfg
            .core
            .stats()
            .record_hydration(&rq.path, &rq.image, size, SystemTime::now());
    }
    let mut stream = match sh.cfg.core.read_content(rq.content).await {
        Ok(s) => s,
        Err(_) => return fail_rest(at, ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL),
    };
    let mut buf: Vec<u8> = Vec::with_capacity(TRANSFER_BUFFER);
    let mut pos = 0u64;
    loop {
        if rq.cancel.load(Ordering::SeqCst) || sh.stopping.load(Ordering::SeqCst) {
            return fail_rest(at, ffi::STATUS_CLOUD_FILE_REQUEST_CANCELED);
        }
        let piece = match stream.next().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(_) => return fail_rest(at, ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL),
        };
        let start = pos;
        pos += piece.len() as u64;
        if pos <= rq.offset {
            continue;
        }
        let lo = usize::try_from(rq.offset.saturating_sub(start)).unwrap_or(0);
        let mut part = &piece[lo..];
        while !part.is_empty() {
            let n = (TRANSFER_BUFFER - buf.len()).min(part.len());
            buf.extend_from_slice(&part[..n]);
            part = &part[n..];
            if buf.len() == TRANSFER_BUFFER {
                if transfer_data(rq.keys, Some(&buf), at, buf.len() as u64, STATUS_SUCCESS).is_err()
                {
                    return fail_rest(at, ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
                }
                at += buf.len() as u64;
                buf.clear();
            }
        }
    }
    if !buf.is_empty() {
        if transfer_data(rq.keys, Some(&buf), at, buf.len() as u64, STATUS_SUCCESS).is_err() {
            return fail_rest(at, ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
        }
        at += buf.len() as u64;
    }
    // A stream shorter than the size the placeholder says is a failed hydration.
    fail_rest(at, ffi::STATUS_CLOUD_FILE_UNSUCCESSFUL);
}

/// # Safety
/// Called by the filter with valid callback info and parameters.
unsafe extern "system" fn cancel_fetch_data(
    info: *const CF_CALLBACK_INFO,
    _p: *const CF_CALLBACK_PARAMETERS,
) {
    guard(|| {
        // SAFETY: the filter passes a valid pointer for the duration of the callback.
        if let Some((sh, info)) = unsafe { inner_of(info) }
            && let Some(flag) = lock(&sh.transfers).get(&info.TransferKey)
        {
            flag.store(true, Ordering::SeqCst);
        }
    });
}

/// Whether a delete of `path` (or a rename of it to `to`) is refused: a locked path never moves,
/// and nothing moves into a locked path. `to` is `None` for a delete; `Some(None)` is a target
/// outside the root.
pub(crate) fn refused(
    locked: &dyn Fn(&RepoPath) -> bool,
    path: &RepoPath,
    to: Option<Option<&RepoPath>>,
) -> bool {
    locked(path) || matches!(to, Some(Some(t)) if locked(t))
}

/// # Safety
/// Called by the filter with valid callback info and parameters.
unsafe extern "system" fn notify_delete(
    info: *const CF_CALLBACK_INFO,
    p: *const CF_CALLBACK_PARAMETERS,
) {
    guard(|| {
        // SAFETY: the filter passes valid pointers for the duration of the callback.
        let Some((sh, info)) = (unsafe { inner_of(info) }) else {
            return;
        };
        let k = Keys::of(info);
        // SAFETY: the parameters of a `NOTIFY_DELETE` callback, or null.
        let undelete = unsafe { p.as_ref() }.is_some_and(|p| {
            // SAFETY: as above.
            let d = unsafe { p.Anonymous.Delete };
            d.Flags.0 & CF_CALLBACK_DELETE_FLAG_IS_UNDELETE.0 != 0
        });
        let allow = undelete
            || is_own(info)
            || match path_of(sh, info) {
                Some(Some(path)) => !refused(&|p| sh.cfg.core.locked(p), &path, None),
                // The root itself is never deleted while it is served.
                Some(None) => false,
                None => true,
            };
        ack(k, false, allow);
    });
}

/// # Safety
/// Called by the filter with valid callback info and parameters.
unsafe extern "system" fn notify_rename(
    info: *const CF_CALLBACK_INFO,
    p: *const CF_CALLBACK_PARAMETERS,
) {
    guard(|| {
        // SAFETY: the filter passes valid pointers for the duration of the callback.
        let Some((sh, info)) = (unsafe { inner_of(info) }) else {
            return;
        };
        let k = Keys::of(info);
        // SAFETY: the parameters of a `NOTIFY_RENAME` callback, or null.
        let target = unsafe { p.as_ref() }.and_then(|p| {
            // SAFETY: as above; the target path is NUL terminated or null.
            unsafe { ffi::from_wide(p.Anonymous.Rename.TargetPath) }
        });
        let to = target.as_deref().map(|t| sh.repo_path(t).flatten());
        let allow = is_own(info)
            || match path_of(sh, info) {
                Some(Some(path)) => !refused(
                    &|p| sh.cfg.core.locked(p),
                    &path,
                    Some(to.as_ref().and_then(Option::as_ref)),
                ),
                Some(None) => false,
                None => true,
            };
        ack(k, true, allow);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn locked_paths_never_move_and_nothing_moves_into_them() {
        let locked = |p: &RepoPath| p.as_bytes().starts_with(b"sdk");
        assert!(refused(&locked, &rp("sdk/a.h"), None));
        assert!(!refused(&locked, &rp("src/a.rs"), None));
        assert!(refused(
            &locked,
            &rp("src/a.rs"),
            Some(Some(&rp("sdk/a.rs")))
        ));
        assert!(refused(&locked, &rp("sdk/a.h"), Some(None)));
        assert!(!refused(&locked, &rp("src/a.rs"), Some(None)));
        assert!(!refused(
            &locked,
            &rp("src/a.rs"),
            Some(Some(&rp("src/b.rs")))
        ));
    }
}
