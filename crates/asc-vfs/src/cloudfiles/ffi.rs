//! The Cloud Files calls this backend makes, over the `windows` crate: sync root registration and
//! connection, placeholder creation, update, dehydration and revert, and the probe that reads a
//! path's placeholder state and identity. Every handle is closed on drop.

use std::ffi::c_void;
use std::fmt;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FileAttributeTagInfo, GetFileInformationByHandleEx,
    GetLongPathNameW, OPEN_EXISTING, SetFileAttributesW,
};
use windows::core::{GUID, PCWSTR};

use super::Ident;
use crate::win::verbatim;

pub use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_CLOUD_FILE_ACCESS_DENIED, STATUS_CLOUD_FILE_REQUEST_CANCELED,
    STATUS_CLOUD_FILE_UNSUCCESSFUL, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
/// The reparse tag every Cloud Files placeholder carries (`IO_REPARSE_TAG_CLOUD*`): the low 12
/// bits of `0x9000_x01A` vary with the sync root's flags.
pub const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0xFFFF_0FFF;
pub const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;

/// `ERROR_ALREADY_EXISTS` as an `HRESULT`, and as a Win32 code.
pub const HR_ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;
pub const ERROR_ALREADY_EXISTS: i32 = 183;
/// Win32 errors that mean another process holds the file.
pub const ERROR_SHARING_VIOLATION: i32 = 32;
pub const ERROR_LOCK_VIOLATION: i32 = 33;
pub const ERROR_CLOUD_FILE_IN_USE: i32 = 391;

/// Most placeholders one `CfCreatePlaceholders` call is given.
const CREATE_BATCH: usize = 4096;

/// The provider id every sync root of this program is registered with.
pub const PROVIDER_ID: GUID = GUID::from_u128(0x6a5c_0001_0000_4000_8000_0000_0000_a5c1);
/// The provider name a sync root of this program carries.
pub const PROVIDER_NAME: &str = "asc";

/// The Cloud Files API cannot be used here.
#[derive(Debug)]
pub struct ProviderUnavailable(pub String);

impl fmt::Display for ProviderUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cloud Files is unavailable: {}", self.0)
    }
}

impl std::error::Error for ProviderUnavailable {}

/// An `io::Error` that keeps the Win32 code of a `windows` error where there is one.
pub fn io_err(e: &windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(format!("HRESULT {hr:#010x}: {}", e.message()))
    }
}

/// The NUL terminated wide form of `p`, verbatim so long paths work.
pub fn wide(p: &Path) -> Vec<u16> {
    verbatim(p)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The NUL terminated wide form of `p` without the verbatim prefix: sync root registration,
/// connection and lookup compare the root against the filter's own (non-verbatim) spelling.
pub fn plain_wide(p: &Path) -> Vec<u16> {
    plain(p)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `p` without the verbatim prefix.
pub fn plain(p: &Path) -> std::path::PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => std::path::PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// `path` with its 8.3 short names expanded, for the longest leading part that exists (the rest,
/// such as the target of a rename, is kept as given). `None` when no part of it exists.
pub fn long_form(path: &str) -> Option<String> {
    let mut head = path.trim_end_matches('\\');
    let mut tail = String::new();
    loop {
        if let Some(long) = long_path(head) {
            return Some(long + &tail);
        }
        let cut = head.rfind('\\')?;
        tail.insert_str(0, &head[cut..]);
        head = &head[..cut];
        if head.len() <= 2 {
            return None;
        }
    }
}

fn long_path(path: &str) -> Option<String> {
    let w = to_wide(path);
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `w` is NUL terminated; the buffer is writable for its whole length.
        let n = unsafe { GetLongPathNameW(PCWSTR(w.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        if n > 1 << 16 {
            return None;
        }
        buf.resize(n + 1, 0);
    }
}

/// NUL terminated wide string.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// # Safety
/// `p` is null or NUL terminated.
pub unsafe fn from_wide(p: PCWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: per the contract.
    unsafe { p.to_string() }.ok()
}

/// Unix seconds to a `FILETIME` count.
pub fn filetime(t: i64) -> i64 {
    t.saturating_add(11_644_473_600).saturating_mul(10_000_000)
}

/// The metadata a placeholder is created or updated with.
pub fn metadata(dir: bool, size: u64, t: i64, read_only: bool) -> CF_FS_METADATA {
    let ft = filetime(t);
    let attrs = if dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else if read_only {
        FILE_ATTRIBUTE_READONLY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: ft,
            LastAccessTime: ft,
            LastWriteTime: ft,
            ChangeTime: ft,
            FileAttributes: attrs,
        },
        FileSize: if dir {
            0
        } else {
            i64::try_from(size).unwrap_or(i64::MAX)
        },
    }
}

/// Whether the Cloud Files filter is running.
pub fn platform_available() -> Result<(), ProviderUnavailable> {
    // SAFETY: no arguments.
    unsafe { CfGetPlatformInfo() }
        .map(|_| ())
        .map_err(|e| ProviderUnavailable(e.message()))
}

/// An open handle, closed on drop.
pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateFileW` and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: a file handle has no thread affinity.
unsafe impl Send for Handle {}

/// Share modes that let every other opener in.
pub const SHARE_ALL: FILE_SHARE_MODE =
    FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);

/// Opens `p` with `access` and `share`, never following a reparse point and opening directories.
pub fn open(p: &Path, access: u32, share: FILE_SHARE_MODE) -> io::Result<Handle> {
    let w = wide(p);
    // SAFETY: NUL terminated path; the handle is owned by the result.
    unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            access,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(
                FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0,
            ),
            None,
        )
    }
    .map(Handle)
    .map_err(|e| io_err(&e))
}

/// Shows this thread the placeholders as they are on disk while it lives. Without it the filter
/// hides them from a process that is not the connected provider, so after a disconnect a
/// placeholder reads as a plain file.
pub struct Expose(i8);

/// `PHCM_EXPOSE_PLACEHOLDERS`.
const PHCM_EXPOSE: i8 = 2;

impl Expose {
    pub fn new() -> Self {
        // SAFETY: sets a per-thread flag; a negative answer is an error and changes nothing.
        let prev = unsafe {
            windows::Wdk::Storage::FileSystem::RtlSetThreadPlaceholderCompatibilityMode(PHCM_EXPOSE)
        };
        Self(prev)
    }
}

impl Default for Expose {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Expose {
    fn drop(&mut self) {
        if self.0 >= 0 && self.0 != PHCM_EXPOSE {
            // SAFETY: restores the flag this guard replaced.
            unsafe {
                windows::Wdk::Storage::FileSystem::RtlSetThreadPlaceholderCompatibilityMode(self.0);
            }
        }
    }
}

/// Read attributes only: the probe, which never hydrates.
pub fn open_probe(p: &Path) -> io::Result<Handle> {
    open(p, FILE_READ_ATTRIBUTES.0, SHARE_ALL)
}

/// Exclusive write access, as a placeholder update or dehydration needs: a process that has the
/// file open makes this fail with a sharing violation.
pub fn open_exclusive(p: &Path) -> io::Result<Handle> {
    let access: FILE_ACCESS_RIGHTS =
        FILE_ACCESS_RIGHTS(FILE_WRITE_DATA.0 | FILE_WRITE_ATTRIBUTES.0 | FILE_READ_ATTRIBUTES.0);
    open(p, access.0, FILE_SHARE_MODE(0))
}

/// Whether an error means another process holds the path.
pub fn is_in_use(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_CLOUD_FILE_IN_USE)
    )
}

/// What a path is to the filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    pub attrs: u32,
    /// `CF_PLACEHOLDER_STATE` bits.
    pub state: u32,
    /// Bytes of the file on disk (0 for an empty placeholder).
    pub on_disk: u64,
    pub in_sync: bool,
    /// The identity bytes; empty for something that is not a placeholder.
    pub identity: Vec<u8>,
}

impl Probe {
    pub fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    pub fn is_placeholder(&self) -> bool {
        self.state & CF_PLACEHOLDER_STATE_PLACEHOLDER.0 != 0
            && self.state != CF_PLACEHOLDER_STATE_INVALID.0
    }

    /// A reparse point that is not a placeholder (symbolic link, junction, mount point).
    pub fn is_foreign_reparse(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !self.is_placeholder()
    }

    /// A directory whose children are not all on disk yet.
    pub fn is_unpopulated(&self) -> bool {
        self.is_dir()
            && self.is_placeholder()
            && (self.state & CF_PLACEHOLDER_STATE_PARTIAL.0 != 0
                || self.attrs & FILE_ATTRIBUTE_RECALL_ON_OPEN != 0)
    }

    pub fn ident(&self) -> Option<Ident> {
        Ident::decode(&self.identity)
    }
}

/// Reads the state of `p`. `NotFound` when nothing is there on disk.
pub fn probe(p: &Path) -> io::Result<Probe> {
    let _expose = Expose::new();
    let h = open_probe(p)?;
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: a valid handle and an out buffer of the class's size.
    unsafe {
        GetFileInformationByHandleEx(
            h.0,
            FileAttributeTagInfo,
            (&raw mut tag).cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|e| io_err(&e))?;
    // SAFETY: plain integers.
    let state =
        unsafe { CfGetPlaceholderStateFromAttributeTag(tag.FileAttributes, tag.ReparseTag) };
    let mut out = Probe {
        attrs: tag.FileAttributes,
        state: state.0,
        ..Probe::default()
    };
    if out.is_placeholder() {
        // The standard info with an identity of up to 4 KiB (the API's limit).
        let mut buf = vec![0u64; 520];
        let mut got = 0u32;
        // SAFETY: a valid handle and a buffer of the stated length.
        let r = unsafe {
            CfGetPlaceholderInfo(
                h.0,
                CF_PLACEHOLDER_INFO_STANDARD,
                buf.as_mut_ptr().cast::<c_void>(),
                (buf.len() * 8) as u32,
                Some(&raw mut got),
            )
        };
        if r.is_ok() {
            // SAFETY: the buffer is 8-byte aligned and holds a `CF_PLACEHOLDER_STANDARD_INFO`.
            let s = unsafe { &*buf.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
            out.on_disk = u64::try_from(s.OnDiskDataSize).unwrap_or(0);
            out.in_sync = s.InSyncState == CF_IN_SYNC_STATE_IN_SYNC;
            let head = std::mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
            let len = (s.FileIdentityLength as usize).min(buf.len() * 8 - head);
            // SAFETY: `head + len` is within the buffer.
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>().add(head), len) };
            out.identity = bytes.to_vec();
        }
    }
    Ok(out)
}

/// `probe`, with `None` for a path with nothing on disk.
pub fn probe_opt(p: &Path) -> io::Result<Option<Probe>> {
    match probe(p) {
        Ok(pr) => Ok(Some(pr)),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `NotFound` and `ERROR_PATH_NOT_FOUND`.
pub fn is_not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || matches!(e.raw_os_error(), Some(2 | 3))
}

/// Registers `root` as a sync root of this program for `workspace`. Directories populate on
/// demand, files hydrate fully on first read, and every change another process makes clears the
/// in-sync state.
pub fn register(root: &Path, workspace: &[u8; 16]) -> io::Result<()> {
    let w = plain_wide(root);
    let name = to_wide(PROVIDER_NAME);
    let version = to_wide(env!("CARGO_PKG_VERSION"));
    let reg = CF_SYNC_REGISTRATION {
        StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: workspace.as_ptr().cast(),
        SyncRootIdentityLength: workspace.len() as u32,
        FileIdentity: super::ROOT_IDENTITY.as_ptr().cast(),
        FileIdentityLength: super::ROOT_IDENTITY.len() as u32,
        ProviderId: PROVIDER_ID,
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_PRIMARY(CF_HYDRATION_POLICY_FULL.0),
            Modifier: CF_HYDRATION_POLICY_MODIFIER(0),
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_PRIMARY(CF_POPULATION_POLICY_FULL.0),
            Modifier: CF_POPULATION_POLICY_MODIFIER(0),
        },
        // Every change by another process clears the in-sync state; this program's own attribute
        // and placeholder changes (a switch, a dehydration) keep it.
        InSync: CF_INSYNC_POLICY(
            CF_INSYNC_POLICY_TRACK_ALL.0 | CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE.0,
        ),
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    // SAFETY: valid NUL terminated strings and structures that outlive the call.
    unsafe { CfRegisterSyncRoot(PCWSTR(w.as_ptr()), &reg, &policies, CF_REGISTER_FLAG_UPDATE) }
        .map_err(|e| io_err(&e))
}

/// Removes the sync root registration of `root`.
pub fn unregister(root: &Path) -> io::Result<()> {
    let w = plain_wide(root);
    // SAFETY: NUL terminated path.
    unsafe { CfUnregisterSyncRoot(PCWSTR(w.as_ptr())) }.map_err(|e| io_err(&e))
}

/// The provider name of the sync root `p` is in, or `None` where it is in none.
pub fn sync_root_provider(p: &Path) -> Option<String> {
    let w = plain_wide(p);
    let mut info = CF_SYNC_ROOT_PROVIDER_INFO {
        ProviderStatus: CF_SYNC_PROVIDER_STATUS(0),
        ProviderName: [0; 256],
        ProviderVersion: [0; 256],
    };
    // SAFETY: NUL terminated path and an out buffer of the class's size.
    let r = unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(w.as_ptr()),
            CF_SYNC_ROOT_INFO_PROVIDER,
            (&raw mut info).cast::<c_void>(),
            size_of::<CF_SYNC_ROOT_PROVIDER_INFO>() as u32,
            None,
        )
    };
    r.ok()?;
    let end = info
        .ProviderName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(256);
    Some(String::from_utf16_lossy(&info.ProviderName[..end]))
}

/// Whether `root` is a sync root this program registered.
pub fn is_our_sync_root(root: &Path) -> bool {
    sync_root_provider(root).is_some_and(|n| n == PROVIDER_NAME)
}

/// The sync root identity of the sync root holding `root` (the workspace uuid for this program's
/// roots), or `None` when `root` is in none.
pub fn sync_root_identity(root: &Path) -> Option<Vec<u8>> {
    const MAX_IDENTITY: usize = 64;
    let head = std::mem::offset_of!(CF_SYNC_ROOT_STANDARD_INFO, SyncRootIdentity);
    // `u64` keeps the buffer aligned for the structure.
    let mut buf = vec![0u64; (head + MAX_IDENTITY).div_ceil(8)];
    let w = plain_wide(root);
    let mut len = 0u32;
    // SAFETY: NUL terminated path and an out buffer of the size given.
    unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(w.as_ptr()),
            CF_SYNC_ROOT_INFO_STANDARD,
            buf.as_mut_ptr().cast::<c_void>(),
            (buf.len() * 8) as u32,
            Some(&raw mut len),
        )
    }
    .ok()?;
    // SAFETY: the buffer is aligned and at least the structure's size.
    let info = unsafe { &*buf.as_ptr().cast::<CF_SYNC_ROOT_STANDARD_INFO>() };
    let n = (info.SyncRootIdentityLength as usize).min(MAX_IDENTITY);
    // SAFETY: `n` bytes of identity follow the header inside the buffer.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>().add(head), n) };
    Some(bytes.to_vec())
}

/// One placeholder to create.
pub struct NewPlaceholder {
    pub name: String,
    pub identity: Vec<u8>,
    pub meta: CF_FS_METADATA,
    pub dir: bool,
}

impl NewPlaceholder {
    /// The flags a placeholder is created with: in sync, and a directory lists on demand.
    pub fn flags(&self) -> CF_PLACEHOLDER_CREATE_FLAGS {
        CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
    }
}

/// The create infos of `items`; the wide names are kept alive in `names`.
pub fn create_infos(
    items: &[NewPlaceholder],
    names: &mut Vec<Vec<u16>>,
) -> Vec<CF_PLACEHOLDER_CREATE_INFO> {
    names.clear();
    names.extend(items.iter().map(|i| to_wide(&i.name)));
    items
        .iter()
        .zip(names.iter())
        .map(|(i, n)| CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(n.as_ptr()),
            FsMetadata: i.meta,
            FileIdentity: i.identity.as_ptr().cast(),
            FileIdentityLength: i.identity.len() as u32,
            Flags: i.flags(),
            ..Default::default()
        })
        .collect()
}

/// Creates `items` in the directory `base`; a name that already exists is left as it is.
pub fn create_placeholders(base: &Path, items: &[NewPlaceholder]) -> io::Result<usize> {
    let w = wide(base);
    let mut made = 0usize;
    for batch in items.chunks(CREATE_BATCH) {
        let mut names = Vec::new();
        let mut infos = create_infos(batch, &mut names);
        let mut done = 0u32;
        // SAFETY: NUL terminated base; every pointer in `infos` points into `names` or `batch`,
        // both alive for the call.
        let r = unsafe {
            CfCreatePlaceholders(
                PCWSTR(w.as_ptr()),
                &mut infos,
                CF_CREATE_FLAG_NONE,
                Some(&raw mut done),
            )
        };
        let mut first_err = None;
        for i in &infos {
            if i.Result.is_ok() {
                made += 1;
            } else if i.Result.0 != HR_ALREADY_EXISTS && first_err.is_none() {
                first_err = Some(windows::core::Error::from_hresult(i.Result));
            }
        }
        if let Err(e) = r
            && e.code().0 != HR_ALREADY_EXISTS
            && first_err.is_none()
        {
            first_err = Some(e);
        }
        if let Some(e) = first_err {
            return Err(io_err(&e));
        }
    }
    Ok(made)
}

/// Updates the placeholder open as `h`: identity, metadata (or none) and `flags`.
pub fn update(
    h: &Handle,
    identity: Option<&[u8]>,
    meta: Option<&CF_FS_METADATA>,
    flags: CF_UPDATE_FLAGS,
) -> io::Result<()> {
    let (id_ptr, id_len) = match identity {
        Some(b) => (Some(b.as_ptr().cast::<c_void>()), b.len() as u32),
        None => (None, 0),
    };
    // SAFETY: a valid handle; the identity and metadata outlive the call.
    unsafe {
        CfUpdatePlaceholder(
            h.0,
            meta.map(std::ptr::from_ref),
            id_ptr,
            id_len,
            None,
            flags,
            None,
            None,
        )
    }
    .map_err(|e| io_err(&e))
}

/// Turns the directory at `p` into one whose children are all on disk: no more `FETCH_PLACEHOLDERS`.
pub fn disable_on_demand(p: &Path) -> io::Result<()> {
    let h = open(
        p,
        FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0,
        SHARE_ALL,
    )?;
    update(&h, None, None, CF_UPDATE_FLAG_DISABLE_ON_DEMAND_POPULATION)
}

/// Turns the plain file or directory at `p` into a placeholder with `identity`, marked in sync
/// when `in_sync`; a directory with `on_demand` lists the children it lacks on demand. A
/// read-only file keeps its attribute.
pub fn convert(p: &Path, identity: &[u8], in_sync: bool, on_demand: bool) -> io::Result<()> {
    let attrs = std::fs::symlink_metadata(p)
        .map(|m| std::os::windows::fs::MetadataExt::file_attributes(&m))
        .unwrap_or(0);
    let cleared = clear_read_only(p, attrs)?;
    let done = (|| {
        let h = open(
            p,
            FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0,
            SHARE_ALL,
        )?;
        let mut flags = CF_CONVERT_FLAG_NONE.0;
        if in_sync {
            flags |= CF_CONVERT_FLAG_MARK_IN_SYNC.0;
        }
        if on_demand {
            flags |= CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION.0;
        }
        // SAFETY: a valid handle; the identity outlives the call.
        unsafe {
            CfConvertToPlaceholder(
                h.0,
                Some(identity.as_ptr().cast::<c_void>()),
                identity.len() as u32,
                CF_CONVERT_FLAGS(flags),
                None,
                None,
            )
        }
        .map_err(|e| io_err(&e))
    })();
    if cleared {
        set_attributes(p, attrs)?;
    }
    done
}

/// Drops the data of the file open as `h`, keeping the placeholder.
pub fn dehydrate(h: &Handle) -> io::Result<()> {
    // SAFETY: a valid handle.
    unsafe { CfDehydratePlaceholder(h.0, 0, -1, CF_DEHYDRATE_FLAG_NONE, None) }
        .map_err(|e| io_err(&e))
}

/// Turns the placeholder open as `h` back into a plain file or directory.
pub fn revert(h: &Handle) -> io::Result<()> {
    // SAFETY: a valid handle.
    unsafe { CfRevertPlaceholder(h.0, CF_REVERT_FLAG_NONE, None) }.map_err(|e| io_err(&e))
}

/// Sets the attributes of `p`.
pub fn set_attributes(p: &Path, attrs: u32) -> io::Result<()> {
    let w = wide(p);
    // SAFETY: NUL terminated path.
    unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attrs)) }
        .map_err(|e| io_err(&e))
}

/// Clears `FILE_ATTRIBUTE_READONLY` of `p` if set; returns whether it was set.
pub fn clear_read_only(p: &Path, attrs: u32) -> io::Result<bool> {
    if attrs & FILE_ATTRIBUTE_READONLY == 0 {
        return Ok(false);
    }
    let rest = attrs & !FILE_ATTRIBUTE_READONLY;
    set_attributes(
        p,
        if rest == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            rest
        },
    )?;
    Ok(true)
}

/// An NTSTATUS a callback completes an operation with.
pub fn status(code: NTSTATUS) -> NTSTATUS {
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 8.3 form of an existing path, where the volume makes them.
    fn short(p: &str) -> Option<String> {
        let w = to_wide(p);
        let mut buf = vec![0u16; 1024];
        // SAFETY: `w` is NUL terminated; the buffer is writable for its whole length.
        let n = unsafe {
            windows::Win32::Storage::FileSystem::GetShortPathNameW(
                PCWSTR(w.as_ptr()),
                Some(&mut buf),
            )
        } as usize;
        (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
    }

    #[test]
    fn long_form_expands_short_names_and_keeps_a_missing_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a directory with a long name");
        std::fs::create_dir(&dir).unwrap();
        // The temporary directory may itself be spelled with short names.
        let long = plain(&std::fs::canonicalize(&dir).unwrap())
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(long_form(&long).as_deref(), Some(&long[..]));
        let gone = format!(r"{long}\not there\x.txt");
        assert_eq!(long_form(&gone), Some(gone.clone()));
        assert_eq!(long_form(r"Q:\surely\absent"), None);
        // A volume without 8.3 names has nothing to expand.
        if let Some(s) = short(&long).filter(|s| s.contains('~')) {
            assert_eq!(
                long_form(&s).map(|l| l.to_lowercase()),
                Some(long.to_lowercase())
            );
            let tail = format!(r"{s}\new.txt");
            assert_eq!(
                long_form(&tail).map(|l| l.to_lowercase()),
                Some(format!(r"{long}\new.txt").to_lowercase())
            );
        }
    }

    #[test]
    fn filetime_is_the_windows_epoch_in_100ns() {
        assert_eq!(filetime(0), 116_444_736_000_000_000);
        assert_eq!(filetime(1), 116_444_736_010_000_000);
    }

    #[test]
    fn metadata_follows_the_node() {
        let d = metadata(true, 9, 0, true);
        assert_eq!(
            (d.FileSize, d.BasicInfo.FileAttributes),
            (0, FILE_ATTRIBUTE_DIRECTORY)
        );
        let f = metadata(false, 9, 0, false);
        assert_eq!(
            (f.FileSize, f.BasicInfo.FileAttributes),
            (9, FILE_ATTRIBUTE_NORMAL)
        );
        let r = metadata(false, 9, 0, true);
        assert_eq!(r.BasicInfo.FileAttributes, FILE_ATTRIBUTE_READONLY);
    }

    #[test]
    fn win32_codes_survive_the_conversion() {
        let e = windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0020_u32 as i32));
        assert!(is_in_use(&io_err(&e)));
        let e = windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0002_u32 as i32));
        assert!(is_not_found(&io_err(&e)));
    }

    #[test]
    fn a_plain_directory_probes_as_no_placeholder() {
        let d = crate::win::testkit::tempdir();
        let p = probe(d.path()).unwrap();
        assert!(p.is_dir() && !p.is_placeholder() && !p.is_unpopulated());
        assert!(probe_opt(&d.path().join("absent")).unwrap().is_none());
        assert!(!is_our_sync_root(d.path()));
    }
}
