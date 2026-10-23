//! Placeholder and file data callbacks (SPEC §10.2 items 3-5). `GetFileData` serves the ContentID
//! recorded in the placeholder, never the current HEAD, in 1 MiB aligned writes. A fetch that is
//! not local completes asynchronously (`ERROR_IO_PENDING` + `PrjCompleteCommand`), tracked per
//! command id so a cancel stops it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use asc_core::codec::decode_envelope;
use asc_core::id::FileId;
use asc_core::object::{ChunkList, Kind};
use asc_core::path::RepoPath;
use asc_store::key::ObjKey;

use super::enumerate::basic_info;
use super::ffi::{self, CallbackData, Ctx, Guid, Hresult, ID_LEN, PlaceholderInfo, VersionInfo};
use super::provider::{Inner, lock};
use super::{block, guard, rel_path};
use crate::core::{ContentRef, VfsError};
use crate::policy::Decision;

/// Bytes per `PrjWriteFileData` call.
pub const WRITE_BUFFER: usize = 1 << 20;
/// `ContentID` kinds (SPEC §10.2 item 3; kind 2 is unused).
pub const CONTENT_FILE: u8 = 1;
pub const CONTENT_TREE: u8 = 3;
/// Length of a `ContentID` that this provider writes: `kind ‖ id`.
pub const CONTENT_ID_LEN: usize = 33;

/// The shared state and callback data of a callback.
///
/// # Safety
/// `d` is null or the pointer ProjFS passed, valid for the callback.
pub(crate) unsafe fn inner_of<'a>(d: *const CallbackData) -> Option<(&'a Inner, &'a CallbackData)> {
    // SAFETY: per the contract.
    let d = unsafe { d.as_ref() }?;
    // SAFETY: `instance_context` is the `Arc::into_raw` pointer given to `PrjStartVirtualizing`,
    // alive until after `PrjStopVirtualizing` returns.
    let sh = unsafe { d.instance_context.cast::<Inner>().as_ref() }?;
    Some((sh, d))
}

/// An owning handle on the shared state for asynchronous work.
///
/// # Safety
/// As [`inner_of`].
unsafe fn arc_of(d: &CallbackData) -> Option<Arc<Inner>> {
    let p = d.instance_context.cast::<Inner>().cast_const();
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` came from `Arc::into_raw` and the count is raised before it is adopted.
    unsafe {
        Arc::increment_strong_count(p);
        Some(Arc::from_raw(p))
    }
}

fn path_of(d: &CallbackData) -> Result<Option<RepoPath>, Hresult> {
    // SAFETY: the pointer is NUL terminated or null.
    let w = unsafe { ffi::wide_to_string(d.file_path_name) }.ok_or(ffi::ERROR_INVALID_PARAMETER)?;
    rel_path(&w).map_err(|()| ffi::ERROR_FILE_NOT_FOUND)
}

/// The `ContentID` that a placeholder of `kind` and `id` carries.
pub(crate) fn content_id(kind: u8, id: &[u8; 32]) -> [u8; ID_LEN] {
    let mut c = [0u8; ID_LEN];
    c[0] = kind;
    c[1..CONTENT_ID_LEN].copy_from_slice(id);
    c
}

/// Parses a placeholder's `ContentID` into a file id.
pub(crate) fn file_of_content(c: &[u8; ID_LEN]) -> Option<FileId> {
    if c[0] != CONTENT_FILE || c[CONTENT_ID_LEN..].iter().any(|&b| b != 0) {
        return None;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&c[1..CONTENT_ID_LEN]);
    Some(FileId::from_bytes(id))
}

fn hresult_of(e: &VfsError) -> Hresult {
    match e {
        VfsError::Denied(_) => ffi::ERROR_ACCESS_DENIED,
        VfsError::NotFound(_) | VfsError::NotAFile(_) => ffi::ERROR_FILE_NOT_FOUND,
        VfsError::BadPath => ffi::ERROR_INVALID_PARAMETER,
        _ => ffi::ERROR_IO_DEVICE,
    }
}

/// # Safety
/// Called by ProjFS with a valid callback data.
pub(crate) unsafe extern "system" fn get_placeholder(d: *const CallbackData) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes a valid pointer for the duration of the callback.
        let Some((sh, d)) = (unsafe { inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        let path = match path_of(d) {
            Ok(Some(p)) => p,
            Ok(None) => return ffi::ERROR_FILE_NOT_FOUND,
            Err(hr) => return hr,
        };
        // Case-insensitive lookup; the tree's own spelling is in `node.path`.
        let node = match block(&sh.cfg.rt, sh.cfg.core.resolve(&path)) {
            Ok(Some(n)) => n,
            Ok(None) => return ffi::ERROR_FILE_NOT_FOUND,
            Err(e) => return hresult_of(&e),
        };
        let t = match sh.cfg.times.times(std::slice::from_ref(&node.path)) {
            Ok(v) if v.len() == 1 => v[0],
            _ => return ffi::ERROR_IO_DEVICE,
        };
        let mut info = PlaceholderInfo::zeroed();
        info.basic = basic_info(&node, t);
        info.version = VersionInfo {
            provider_id: {
                let mut p = [0u8; ID_LEN];
                p[..16].copy_from_slice(&sh.cfg.workspace);
                p
            },
            content_id: match node.kind {
                Kind::Dir | Kind::Gitlink => content_id(CONTENT_TREE, &node.id),
                _ => content_id(CONTENT_FILE, &node.id),
            },
        };
        // SAFETY: the context belongs to this callback; the path is NUL terminated; the info
        // is a full `PRJ_PLACEHOLDER_INFO`.
        unsafe {
            (sh.api.write_placeholder)(
                d.ctx,
                d.file_path_name,
                &info,
                u32::try_from(size_of::<PlaceholderInfo>()).unwrap_or(0),
            )
        }
    })
}

/// # Safety
/// Called by ProjFS with a valid callback data.
pub(crate) unsafe extern "system" fn query_file_name(d: *const CallbackData) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes a valid pointer for the duration of the callback.
        let Some((sh, d)) = (unsafe { inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        match path_of(d) {
            Ok(None) => ffi::S_OK,
            Ok(Some(p)) => match block(&sh.cfg.rt, sh.cfg.core.resolve(&p)) {
                Ok(Some(_)) => ffi::S_OK,
                Ok(None) => ffi::ERROR_FILE_NOT_FOUND,
                Err(e) => hresult_of(&e),
            },
            Err(hr) => hr,
        }
    })
}

/// # Safety
/// Called by ProjFS with a valid callback data.
pub(crate) unsafe extern "system" fn cancel(d: *const CallbackData) {
    let _ = guard(|| {
        // SAFETY: ProjFS passes a valid pointer for the duration of the callback.
        if let Some((sh, d)) = unsafe { inner_of(d) }
            && let Some(flag) = lock(&sh.commands).get(&d.command_id)
        {
            flag.store(true, Ordering::SeqCst);
        }
        ffi::S_OK
    });
}

/// One `GetFileData` request, owned so it can outlive the callback.
struct Request {
    ctx: usize,
    stream: Guid,
    command: i32,
    path: RepoPath,
    id: FileId,
    offset: u64,
    length: u32,
    image: String,
    cancel: Arc<AtomicBool>,
}

/// # Safety
/// Called by ProjFS with a valid callback data.
pub(crate) unsafe extern "system" fn get_file_data(
    d: *const CallbackData,
    offset: u64,
    length: u32,
) -> Hresult {
    guard(|| {
        // SAFETY: ProjFS passes a valid pointer for the duration of the callback.
        let Some((sh, cb)) = (unsafe { inner_of(d) }) else {
            return ffi::E_FAIL;
        };
        let path = match path_of(cb) {
            Ok(Some(p)) => p,
            Ok(None) => return ffi::ERROR_INVALID_PARAMETER,
            Err(hr) => return hr,
        };
        // SAFETY: `version_info` is null or valid for the callback.
        let Some(v) = (unsafe { cb.version_info.as_ref() }) else {
            return ffi::ERROR_INVALID_PARAMETER;
        };
        // The placeholder says which content it is; HEAD is never consulted.
        let Some(id) = file_of_content(&v.content_id) else {
            return ffi::ERROR_INVALID_PARAMETER;
        };
        if v.provider_id[..16] != sh.cfg.workspace || v.provider_id[16..].iter().any(|&b| b != 0) {
            return ffi::ERROR_INVALID_PARAMETER;
        }
        // SAFETY: NUL terminated or null.
        let image = unsafe { ffi::wide_to_string(cb.triggering_process_image) }.unwrap_or_default();
        // The hydration policy runs before any fetch (SPEC §10.2 item 5).
        if sh.cfg.core.policy().check(&image) == Decision::Deny {
            sh.cfg.core.stats().record_denied(&image);
            return ffi::ERROR_ACCESS_DENIED;
        }
        if length == 0 {
            return ffi::S_OK;
        }
        let rq = Request {
            ctx: cb.ctx as usize,
            stream: cb.data_stream_id,
            command: cb.command_id,
            path,
            id,
            offset,
            length,
            image,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if sh.is_local(id) {
            return match block(&sh.cfg.rt, serve(sh, &rq)) {
                Ok(()) => ffi::S_OK,
                Err(hr) => hr,
            };
        }
        // SAFETY: the callback data is valid and belongs to this instance.
        let Some(arc) = (unsafe { arc_of(cb) }) else {
            return ffi::E_FAIL;
        };
        let Some(guard_) = arc.begin() else {
            return ffi::ERROR_OPERATION_ABORTED;
        };
        lock(&arc.commands).insert(rq.command, rq.cancel.clone());
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let task_arc = arc.clone();
        arc.cfg.rt.spawn(async move {
            // Completing before the callback returned `ERROR_IO_PENDING` is not allowed.
            let _ = go_rx.await;
            let _in_flight = guard_;
            let hr = match serve(&task_arc, &rq).await {
                Ok(()) => ffi::S_OK,
                Err(hr) if rq.cancel.load(Ordering::SeqCst) => {
                    let _ = hr;
                    ffi::ERROR_OPERATION_ABORTED
                }
                Err(hr) => hr,
            };
            lock(&task_arc.commands).remove(&rq.command);
            // SAFETY: the context is valid until `unmount`, which waits for this task.
            unsafe {
                (task_arc.api.complete_command)(rq.ctx as Ctx, rq.command, hr, std::ptr::null());
            }
        });
        let _ = go_tx.send(());
        ffi::ERROR_IO_PENDING
    })
}

/// The size of the content `id` from the object store: a chunk list says it, an inline file is it.
async fn content_of(sh: &Inner, id: FileId) -> Result<(ContentRef, Option<Bytes>), Hresult> {
    let objects = &sh.cfg.objects;
    if let Some(list) = objects
        .get(&ObjKey::ChunkList(id))
        .await
        .map_err(|_| ffi::ERROR_IO_DEVICE)?
    {
        let list = decode_envelope::<ChunkList>(&list).map_err(|_| ffi::ERROR_IO_DEVICE)?;
        return Ok((
            ContentRef {
                id,
                size: list.size,
            },
            None,
        ));
    }
    let data = objects
        .get(&ObjKey::File(id))
        .await
        .map_err(|_| ffi::ERROR_IO_DEVICE)?
        .ok_or(ffi::ERROR_FILE_NOT_FOUND)?;
    Ok((
        ContentRef {
            id,
            size: data.len() as u64,
        },
        Some(data),
    ))
}

/// An aligned ProjFS buffer, freed on drop.
struct Aligned<'a> {
    api: &'a ffi::Api,
    ptr: *mut u8,
}

impl Aligned<'_> {
    /// The buffer start. Accessed through a method so closures capture the whole (Send) value.
    fn base(&self) -> *mut u8 {
        self.ptr
    }
}

// SAFETY: the buffer is plain memory owned by this value; ProjFS buffers have no thread affinity.
unsafe impl Send for Aligned<'_> {}
// SAFETY: as above; shared access only reads the pointer value.
unsafe impl Sync for Aligned<'_> {}

impl Drop for Aligned<'_> {
    fn drop(&mut self) {
        // SAFETY: allocated by `PrjAllocateAlignedBuffer`.
        unsafe { (self.api.free_aligned)(self.ptr.cast()) };
    }
}

/// Writes the requested range of the placeholder's content. Verification is done by the object
/// store (every inline file and chunk is checked against its id) and, for chunked files read to
/// the end, by the assembler.
async fn serve(sh: &Inner, rq: &Request) -> Result<(), Hresult> {
    let (c, inline) = content_of(sh, rq.id).await?;
    if rq.offset >= c.size {
        return Ok(());
    }
    let end = rq.offset.saturating_add(u64::from(rq.length)).min(c.size);
    if rq.offset == 0 {
        sh.cfg
            .core
            .stats()
            .record_hydration(&rq.path, &rq.image, c.size, SystemTime::now());
    }
    let cap = usize::try_from(end - rq.offset).map_or(WRITE_BUFFER, |n| n.min(WRITE_BUFFER));
    // SAFETY: the context is valid for the life of this request (unmount waits for it).
    let raw = unsafe { (sh.api.alloc_aligned)(rq.ctx as Ctx, cap) };
    if raw.is_null() {
        return Err(ffi::ERROR_OUTOFMEMORY);
    }
    let buf = Aligned {
        api: &sh.api,
        ptr: raw.cast(),
    };
    let mut fill = 0usize;
    let mut at = rq.offset;
    let mut pos = 0u64;

    let flush = |fill: &mut usize, at: &mut u64| -> Result<(), Hresult> {
        if *fill == 0 {
            return Ok(());
        }
        if rq.cancel.load(Ordering::SeqCst) || sh.stopping.load(Ordering::SeqCst) {
            return Err(ffi::ERROR_OPERATION_ABORTED);
        }
        // SAFETY: `buf` holds `fill` initialised bytes; offset and length follow ProjFS's
        // alignment rule (whole buffers, then the tail to the requested end).
        let hr = unsafe {
            (sh.api.write_file_data)(
                rq.ctx as Ctx,
                &rq.stream,
                buf.base().cast(),
                *at,
                u32::try_from(*fill).unwrap_or(0),
            )
        };
        if hr != ffi::S_OK {
            return Err(hr);
        }
        *at += *fill as u64;
        *fill = 0;
        Ok(())
    };

    let feed =
        |piece: &[u8], fill: &mut usize, at: &mut u64, pos: &mut u64| -> Result<bool, Hresult> {
            let start = *pos;
            *pos += piece.len() as u64;
            let lo = rq.offset.saturating_sub(start).min(piece.len() as u64);
            let hi = end.saturating_sub(start).min(piece.len() as u64);
            if lo < hi {
                let mut part = &piece[lo as usize..hi as usize];
                while !part.is_empty() {
                    let n = (cap - *fill).min(part.len());
                    // SAFETY: `fill + n <= cap`, the allocation size.
                    unsafe {
                        std::ptr::copy_nonoverlapping(part.as_ptr(), buf.base().add(*fill), n);
                    }
                    *fill += n;
                    part = &part[n..];
                    if *fill == cap {
                        flush(fill, at)?;
                    }
                }
            }
            Ok(*pos >= end)
        };

    if let Some(data) = inline {
        feed(&data, &mut fill, &mut at, &mut pos)?;
    } else {
        let mut stream = sh
            .cfg
            .core
            .read_content(c)
            .await
            .map_err(|e| hresult_of(&e))?;
        loop {
            if rq.cancel.load(Ordering::SeqCst) {
                return Err(ffi::ERROR_OPERATION_ABORTED);
            }
            let Some(piece) = stream.next().await.map_err(|e| hresult_of(&e))? else {
                break;
            };
            if feed(&piece, &mut fill, &mut at, &mut pos)? {
                break;
            }
        }
    }
    flush(&mut fill, &mut at)?;
    if at != end {
        return Err(ffi::ERROR_IO_DEVICE);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_id_round_trips() {
        let id = [7u8; 32];
        let c = content_id(CONTENT_FILE, &id);
        assert_eq!(file_of_content(&c).map(|f| *f.as_bytes()), Some(id));
        assert!(file_of_content(&content_id(CONTENT_TREE, &id)).is_none());
        assert!(file_of_content(&content_id(2, &id)).is_none());
        let mut junk = content_id(CONTENT_FILE, &id);
        junk[40] = 1;
        assert!(file_of_content(&junk).is_none());
    }

    #[test]
    fn errors_map_to_win32() {
        assert_eq!(
            hresult_of(&VfsError::Denied("x".into())),
            ffi::ERROR_ACCESS_DENIED
        );
        assert_eq!(hresult_of(&VfsError::NotMounted), ffi::ERROR_IO_DEVICE);
    }
}
