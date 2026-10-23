//! The thin binding to `ProjectedFSLib.dll` (SPEC §10.2 item 1). The library is loaded at run
//! time and every function pointer is resolved up front, so a host without `Client-ProjFS` gets
//! [`ProviderUnavailable`] instead of a loader failure at process start. The structs mirror
//! `projectedfslib.h` for x64 and arm64 (both are LLP64 with natural alignment).

use std::ffi::c_void;
use std::fmt;

use libloading::Library;

pub type Hresult = i32;
/// `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT`.
pub type Ctx = *mut c_void;
/// `PRJ_DIR_ENTRY_BUFFER_HANDLE`.
pub type DirBuf = *mut c_void;
pub type Pcwstr = *const u16;

pub const S_OK: Hresult = 0;
pub const E_FAIL: Hresult = 0x8000_4005_u32 as i32;

const fn win32(code: u32) -> Hresult {
    // HRESULT_FROM_WIN32 for a non-zero code.
    ((code & 0xFFFF) | 0x8007_0000) as i32
}

pub const ERROR_FILE_NOT_FOUND: Hresult = win32(2);
pub const ERROR_PATH_NOT_FOUND: Hresult = win32(3);
pub const ERROR_ACCESS_DENIED: Hresult = win32(5);
pub const ERROR_OUTOFMEMORY: Hresult = win32(14);
pub const ERROR_INVALID_PARAMETER: Hresult = win32(87);
pub const ERROR_INSUFFICIENT_BUFFER: Hresult = win32(122);
pub const ERROR_IO_DEVICE: Hresult = win32(1117);
pub const ERROR_IO_PENDING: Hresult = win32(997);
pub const ERROR_OPERATION_ABORTED: Hresult = win32(995);
pub const ERROR_REPARSE_POINT_ENCOUNTERED: Hresult = win32(4395);

/// `PRJ_FLAG_USE_NEGATIVE_PATH_CACHE`.
pub const START_USE_NEGATIVE_PATH_CACHE: u32 = 0x1;

/// `PRJ_CALLBACK_DATA_FLAGS`.
pub const CB_ENUM_RESTART_SCAN: u32 = 0x1;
pub const CB_ENUM_RETURN_SINGLE_ENTRY: u32 = 0x2;

/// `PRJ_NOTIFICATION` values (the same bits as `PRJ_NOTIFY_TYPES`).
pub mod notify {
    pub const NONE: u32 = 0;
    pub const FILE_OPENED: u32 = 0x2;
    pub const NEW_FILE_CREATED: u32 = 0x4;
    pub const FILE_OVERWRITTEN: u32 = 0x8;
    pub const PRE_DELETE: u32 = 0x10;
    pub const PRE_RENAME: u32 = 0x20;
    pub const PRE_SET_HARDLINK: u32 = 0x40;
    pub const FILE_RENAMED: u32 = 0x80;
    pub const HARDLINK_CREATED: u32 = 0x100;
    pub const CLOSED_NO_MODIFICATION: u32 = 0x200;
    pub const CLOSED_FILE_MODIFIED: u32 = 0x400;
    pub const CLOSED_FILE_DELETED: u32 = 0x800;
    pub const PRE_CONVERT_TO_FULL: u32 = 0x1000;
}

/// `PRJ_FILE_STATE` bits.
pub mod file_state {
    pub const PLACEHOLDER: u32 = 0x1;
    pub const HYDRATED_PLACEHOLDER: u32 = 0x2;
    pub const DIRTY_PLACEHOLDER: u32 = 0x4;
    pub const FULL: u32 = 0x8;
    pub const TOMBSTONE: u32 = 0x10;
}

/// `PRJ_UPDATE_TYPES` bits.
pub mod update {
    pub const ALLOW_DIRTY_METADATA: u32 = 0x1;
    pub const ALLOW_DIRTY_DATA: u32 = 0x2;
    pub const ALLOW_TOMBSTONE: u32 = 0x4;
    pub const ALLOW_READ_ONLY: u32 = 0x20;
}

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x2000;

/// `PRJ_PLACEHOLDER_ID_LENGTH`.
pub const ID_LEN: usize = 128;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

impl Guid {
    /// The GUID of the RFC 4122 bytes of a UUID.
    pub fn from_uuid_bytes(b: [u8; 16]) -> Self {
        Self {
            d1: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            d2: u16::from_be_bytes([b[4], b[5]]),
            d3: u16::from_be_bytes([b[6], b[7]]),
            d4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VersionInfo {
    pub provider_id: [u8; ID_LEN],
    pub content_id: [u8; ID_LEN],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FileBasicInfo {
    pub is_directory: u8,
    pub file_size: i64,
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EaInfo {
    pub size: u32,
    pub offset: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlaceholderInfo {
    pub basic: FileBasicInfo,
    pub ea: EaInfo,
    pub security: EaInfo,
    pub streams: EaInfo,
    pub version: VersionInfo,
    pub variable_data: [u8; 1],
}

impl PlaceholderInfo {
    pub fn zeroed() -> Self {
        Self {
            basic: FileBasicInfo::default(),
            ea: EaInfo::default(),
            security: EaInfo::default(),
            streams: EaInfo::default(),
            version: VersionInfo {
                provider_id: [0; ID_LEN],
                content_id: [0; ID_LEN],
            },
            variable_data: [0],
        }
    }
}

#[repr(C)]
pub struct CallbackData {
    pub size: u32,
    pub flags: u32,
    pub ctx: Ctx,
    pub command_id: i32,
    pub file_id: Guid,
    pub data_stream_id: Guid,
    pub file_path_name: Pcwstr,
    pub version_info: *const VersionInfo,
    pub triggering_process_id: u32,
    pub triggering_process_image: Pcwstr,
    pub instance_context: *mut c_void,
}

#[repr(C)]
pub struct NotificationMapping {
    pub mask: u32,
    pub root: Pcwstr,
}

#[repr(C)]
pub struct StartOptions {
    pub flags: u32,
    pub pool_threads: u32,
    pub concurrent_threads: u32,
    pub mappings: *mut NotificationMapping,
    pub mapping_count: u32,
}

type StartDirCb = unsafe extern "system" fn(*const CallbackData, *const Guid) -> Hresult;
type EndDirCb = StartDirCb;
type GetDirCb =
    unsafe extern "system" fn(*const CallbackData, *const Guid, Pcwstr, DirBuf) -> Hresult;
type PlaceholderCb = unsafe extern "system" fn(*const CallbackData) -> Hresult;
type FileDataCb = unsafe extern "system" fn(*const CallbackData, u64, u32) -> Hresult;
type NotifyCb =
    unsafe extern "system" fn(*const CallbackData, u8, u32, Pcwstr, *mut c_void) -> Hresult;
type CancelCb = unsafe extern "system" fn(*const CallbackData);

#[repr(C)]
pub struct Callbacks {
    pub start_dir: Option<StartDirCb>,
    pub end_dir: Option<EndDirCb>,
    pub get_dir: Option<GetDirCb>,
    pub get_placeholder: Option<PlaceholderCb>,
    pub get_file_data: Option<FileDataCb>,
    pub query_file_name: Option<PlaceholderCb>,
    pub notification: Option<NotifyCb>,
    pub cancel: Option<CancelCb>,
}

/// `ProjectedFSLib.dll` could not be used.
#[derive(Debug)]
pub struct ProviderUnavailable(pub String);

impl fmt::Display for ProviderUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProjFS is unavailable: {}", self.0)
    }
}

impl std::error::Error for ProviderUnavailable {}

macro_rules! api {
    ($( $field:ident = $sym:literal : fn($($arg:ty),*) $(-> $ret:ty)? ;)*) => {
        /// The resolved ProjFS entry points.
        pub struct Api {
            $( pub $field: unsafe extern "system" fn($($arg),*) $(-> $ret)?, )*
            _lib: Library,
        }

        impl Api {
            /// Loads the library and resolves every function.
            pub fn load() -> Result<Self, ProviderUnavailable> {
                // SAFETY: loading Microsoft's system DLL by its absolute System32 path, so a
                // planted copy in the current or application directory is never picked up.
                let lib = unsafe { Library::new(system_dll_path()) }
                    .map_err(|e| ProviderUnavailable(e.to_string()))?;
                $(
                    // SAFETY: the symbol has the signature in the macro table (projectedfslib.h);
                    // the library stays loaded for as long as `Api` lives.
                    let $field = unsafe {
                        lib.get::<unsafe extern "system" fn($($arg),*) $(-> $ret)?>(
                            concat!($sym, "\x00").as_bytes(),
                        )
                    }
                    .map(|f| *f)
                    .map_err(|e| ProviderUnavailable(format!("{}: {e}", $sym)))?;
                )*
                Ok(Self { $( $field, )* _lib: lib })
            }
        }
    };
}

/// `%SystemRoot%\System32\ProjectedFSLib.dll`.
fn system_dll_path() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::Path::new(&root)
        .join("System32")
        .join("ProjectedFSLib.dll")
}

api! {
    mark_placeholder = "PrjMarkDirectoryAsPlaceholder": fn(Pcwstr, Pcwstr, *const VersionInfo, *const Guid) -> Hresult;
    start = "PrjStartVirtualizing": fn(Pcwstr, *const Callbacks, *const c_void, *const StartOptions, *mut Ctx) -> Hresult;
    stop = "PrjStopVirtualizing": fn(Ctx);
    fill_dir_entry = "PrjFillDirEntryBuffer": fn(Pcwstr, *const FileBasicInfo, DirBuf) -> Hresult;
    write_placeholder = "PrjWritePlaceholderInfo": fn(Ctx, Pcwstr, *const PlaceholderInfo, u32) -> Hresult;
    write_file_data = "PrjWriteFileData": fn(Ctx, *const Guid, *mut c_void, u64, u32) -> Hresult;
    alloc_aligned = "PrjAllocateAlignedBuffer": fn(Ctx, usize) -> *mut c_void;
    free_aligned = "PrjFreeAlignedBuffer": fn(*mut c_void);
    complete_command = "PrjCompleteCommand": fn(Ctx, i32, Hresult, *const c_void) -> Hresult;
    name_match = "PrjFileNameMatch": fn(Pcwstr, Pcwstr) -> u8;
    name_compare = "PrjFileNameCompare": fn(Pcwstr, Pcwstr) -> i32;
    has_wildcards = "PrjDoesNameContainWildCards": fn(Pcwstr) -> u8;
    clear_negative_cache = "PrjClearNegativePathCache": fn(Ctx, *mut u32) -> Hresult;
    on_disk_state = "PrjGetOnDiskFileState": fn(Pcwstr, *mut u32) -> Hresult;
    update_file = "PrjUpdateFileIfNeeded": fn(Ctx, Pcwstr, *const PlaceholderInfo, u32, u32, *mut u32) -> Hresult;
    delete_file = "PrjDeleteFile": fn(Ctx, Pcwstr, u32, *mut u32) -> Hresult;
}

/// `HRESULT_FROM_WIN32(ERROR_SHARING_VIOLATION)`.
pub const ERROR_SHARING_VIOLATION: Hresult = win32(32);
/// `HRESULT_FROM_WIN32(ERROR_DIR_NOT_EMPTY)`.
pub const ERROR_DIR_NOT_EMPTY: Hresult = win32(145);

/// `PRJ_UPDATE_FAILURE_CAUSES`.
pub mod failure {
    pub const DIRTY_METADATA: u32 = 0x1;
    pub const DIRTY_DATA: u32 = 0x2;
    pub const TOMBSTONE: u32 = 0x4;
    pub const READ_ONLY: u32 = 0x8;
}

impl Api {
    /// `PrjUpdateFileIfNeeded` for the root-relative NUL terminated `rel`; the second value is
    /// the failure cause.
    pub fn update(
        &self,
        ctx: usize,
        rel: &[u16],
        info: &PlaceholderInfo,
        flags: u32,
    ) -> (Hresult, u32) {
        assert_eq!(rel.last(), Some(&0), "wide path must be NUL terminated");
        let mut cause = 0u32;
        // SAFETY: `ctx` came from `PrjStartVirtualizing` and is live while the provider is
        // mounted (callers hold the mount); the path is NUL terminated; `info` is a full
        // `PRJ_PLACEHOLDER_INFO` and the size says so.
        let hr = unsafe {
            (self.update_file)(
                ctx as Ctx,
                rel.as_ptr(),
                info,
                u32::try_from(size_of::<PlaceholderInfo>()).unwrap_or(0),
                flags,
                &mut cause,
            )
        };
        (hr, cause)
    }

    /// `PrjDeleteFile` for the root-relative NUL terminated `rel`.
    pub fn delete(&self, ctx: usize, rel: &[u16], flags: u32) -> (Hresult, u32) {
        assert_eq!(rel.last(), Some(&0), "wide path must be NUL terminated");
        let mut cause = 0u32;
        // SAFETY: as `update`.
        let hr = unsafe { (self.delete_file)(ctx as Ctx, rel.as_ptr(), flags, &mut cause) };
        (hr, cause)
    }

    /// `PrjClearNegativePathCache`.
    pub fn clear_negative(&self, ctx: usize) -> Hresult {
        let mut n = 0u32;
        // SAFETY: as `update`.
        unsafe { (self.clear_negative_cache)(ctx as Ctx, &mut n) }
    }

    /// `PrjGetOnDiskFileState` of the NUL terminated absolute `abs`: the `PRJ_FILE_STATE` bits,
    /// or the failing HRESULT (`ERROR_FILE_NOT_FOUND` for a path that is not on disk).
    pub fn file_state(&self, abs: &[u16]) -> Result<u32, Hresult> {
        assert_eq!(abs.last(), Some(&0), "wide path must be NUL terminated");
        let mut st = 0u32;
        // SAFETY: NUL terminated path and a valid out pointer.
        let hr = unsafe { (self.on_disk_state)(abs.as_ptr(), &mut st) };
        if hr == S_OK { Ok(st) } else { Err(hr) }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetFileAttributesW(path: Pcwstr, attrs: u32) -> i32;
}

/// Sets the attributes of the file or directory at the NUL terminated wide `path`.
pub fn set_attributes(path: &[u16], attrs: u32) -> std::io::Result<()> {
    assert_eq!(path.last(), Some(&0), "wide path must be NUL terminated");
    // SAFETY: `path` is NUL terminated (asserted) and outlives the call.
    if unsafe { SetFileAttributesW(path.as_ptr(), attrs) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[repr(C)]
struct FindData {
    attributes: u32,
    times: [u32; 6],
    size_high: u32,
    size_low: u32,
    reserved: [u32; 2],
    name: [u16; 260],
    alternate: [u16; 14],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn FindFirstFileExW(
        name: Pcwstr,
        level: i32,
        data: *mut FindData,
        op: i32,
        filter: *const c_void,
        flags: u32,
    ) -> isize;
    fn FindNextFileW(h: isize, data: *mut FindData) -> i32;
    fn FindClose(h: isize) -> i32;
}

/// `FIND_FIRST_EX_ON_DISK_ENTRIES_ONLY`: a virtualization filter lists only what is on disk.
const FIND_ON_DISK_ENTRIES_ONLY: u32 = 0x4;
const FIND_EX_INFO_BASIC: i32 = 1;
const ERROR_NO_MORE_FILES: i32 = 18;

/// One on-disk directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnDisk {
    pub name: String,
    pub attributes: u32,
    pub size: u64,
}

impl OnDisk {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    pub fn is_reparse(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

/// The entries of the directory at the NUL terminated wide `dir` that are physically on disk
/// (placeholders, hydrated and full files, tombstones); entries that are only projected are
/// not listed and the provider is not asked to enumerate. Stops after `max` entries and
/// reports whether more existed. Names that are not valid UTF-16 are skipped.
pub fn read_dir_on_disk(dir: &[u16], max: usize) -> std::io::Result<(Vec<OnDisk>, bool)> {
    assert_eq!(dir.last(), Some(&0), "wide path must be NUL terminated");
    let mut pattern: Vec<u16> = dir[..dir.len() - 1].to_vec();
    if pattern.last() != Some(&u16::from(b'\\')) {
        pattern.push(u16::from(b'\\'));
    }
    pattern.push(u16::from(b'*'));
    pattern.push(0);
    // SAFETY: all-zero is a valid `FindData` (plain integers).
    let mut data: FindData = unsafe { std::mem::zeroed() };
    // SAFETY: NUL terminated pattern and a valid out buffer.
    let h = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FIND_EX_INFO_BASIC,
            &mut data,
            0,
            std::ptr::null(),
            FIND_ON_DISK_ENTRIES_ONLY,
        )
    };
    if h == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let mut out = Vec::new();
    let more = loop {
        let len = data
            .name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.name.len());
        if let Ok(name) = String::from_utf16(&data.name[..len])
            && name != "."
            && name != ".."
        {
            if out.len() >= max {
                break true;
            }
            out.push(OnDisk {
                name,
                attributes: data.attributes,
                size: (u64::from(data.size_high) << 32) | u64::from(data.size_low),
            });
        }
        // SAFETY: `h` is a live find handle and `data` a valid buffer.
        if unsafe { FindNextFileW(h, &mut data) } == 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: closing the handle opened above.
            unsafe { FindClose(h) };
            return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                Ok((out, false))
            } else {
                Err(e)
            };
        }
    };
    // SAFETY: closing the handle opened above.
    unsafe { FindClose(h) };
    Ok((out, more))
}

/// Most UTF-16 units read from a path the OS hands to a callback (the NTFS limit).
pub const MAX_WIDE: usize = 32_768;

/// Reads a NUL-terminated wide string, bounded by [`MAX_WIDE`]. `None` for null, over-long or
/// unpaired surrogates.
///
/// # Safety
/// `p` is null or points to readable memory up to a NUL terminator or `MAX_WIDE` units.
pub unsafe fn wide_to_string(p: Pcwstr) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    // SAFETY: bounded by MAX_WIDE per the contract.
    while n < MAX_WIDE && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    if n == MAX_WIDE {
        return None;
    }
    // SAFETY: `n` units were just read.
    let units = unsafe { std::slice::from_raw_parts(p, n) };
    String::from_utf16(units).ok()
}

/// A NUL-terminated wide copy of `s`.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated wide copy of a path.
pub fn path_to_wide(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

/// Unix seconds as a `FILETIME` tick count (100 ns since 1601); never negative.
pub fn filetime(unix_secs: i64) -> i64 {
    unix_secs
        .saturating_add(FILETIME_UNIX_OFFSET)
        .max(0)
        .saturating_mul(10_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_header() {
        assert_eq!(size_of::<FileBasicInfo>(), 56);
        assert_eq!(size_of::<VersionInfo>(), 256);
        assert_eq!(std::mem::offset_of!(PlaceholderInfo, version), 80);
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(size_of::<CallbackData>(), 96);
        assert_eq!(std::mem::offset_of!(CallbackData, file_path_name), 56);
        assert_eq!(size_of::<Callbacks>(), 64);
    }

    #[test]
    fn hresults_are_win32_failures() {
        assert_eq!(ERROR_IO_PENDING as u32, 0x8007_03E5);
        assert_eq!(ERROR_FILE_NOT_FOUND as u32, 0x8007_0002);
        assert_eq!(ERROR_INSUFFICIENT_BUFFER as u32, 0x8007_007A);
    }

    #[test]
    fn filetime_epoch() {
        assert_eq!(filetime(0), 116_444_736_000_000_000);
        assert_eq!(filetime(-i64::MAX), 0);
    }

    #[test]
    fn wide_round_trip_and_bounds() {
        let w = to_wide("a\\b");
        // SAFETY: `w` is NUL terminated.
        assert_eq!(
            unsafe { wide_to_string(w.as_ptr()) }.as_deref(),
            Some("a\\b")
        );
        let bad = [0xD800u16, 0];
        // SAFETY: NUL terminated.
        assert_eq!(unsafe { wide_to_string(bad.as_ptr()) }, None);
        // SAFETY: null is allowed.
        assert_eq!(unsafe { wide_to_string(std::ptr::null()) }, None);
    }
}

/// A `ReadDirectoryChangesW` listener, for the tests of the watcher visibility rule (SPEC §10.2.8
/// step 4a).
#[cfg(test)]
pub(crate) mod watch {
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{Pcwstr, path_to_wide};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: Pcwstr,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn ReadDirectoryChangesW(
            h: isize,
            buf: *mut u8,
            len: u32,
            subtree: i32,
            filter: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
            routine: *const c_void,
        ) -> i32;
        fn CancelIoEx(h: isize, overlapped: *const c_void) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }

    const FILE_LIST_DIRECTORY: u32 = 1;
    const SHARE_ALL: u32 = 7;
    const OPEN_EXISTING: u32 = 3;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// File name, directory name, size and last write.
    const FILTER: u32 = 0x1 | 0x2 | 0x8 | 0x10;

    /// `FILE_ACTION_*`.
    pub(crate) const ADDED: u32 = 1;

    pub(crate) struct Watcher {
        h: isize,
        events: Arc<Mutex<Vec<(u32, String)>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Watcher {
        pub(crate) fn new(dir: &std::path::Path) -> std::io::Result<Self> {
            let w = path_to_wide(dir);
            // SAFETY: NUL terminated path; the handle is closed in `drop`.
            let h = unsafe {
                CreateFileW(
                    w.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    SHARE_ALL,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    BACKUP_SEMANTICS,
                    0,
                )
            };
            if h == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let thread = std::thread::spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let mut n = 0u32;
                    // SAFETY: a valid handle and buffer; synchronous, no overlapped.
                    let ok = unsafe {
                        ReadDirectoryChangesW(
                            h,
                            buf.as_mut_ptr(),
                            u32::try_from(buf.len()).unwrap_or(0),
                            0,
                            FILTER,
                            &mut n,
                            std::ptr::null_mut(),
                            std::ptr::null(),
                        )
                    };
                    if ok == 0 || n == 0 {
                        return;
                    }
                    let mut at = 0usize;
                    loop {
                        let rec = &buf[at..];
                        if rec.len() < 12 {
                            break;
                        }
                        let next = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as usize;
                        let action = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
                        let bytes = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
                        let Some(raw) = rec.get(12..12 + bytes) else {
                            break;
                        };
                        let units: Vec<u16> = raw
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        sink.lock()
                            .unwrap()
                            .push((action, String::from_utf16_lossy(&units)));
                        if next == 0 {
                            break;
                        }
                        at += next;
                    }
                }
            });
            Ok(Self {
                h,
                events,
                thread: Some(thread),
            })
        }

        /// Whether `action` for `name` arrives within `timeout`.
        pub(crate) fn wait_for(&self, action: u32, name: &str, timeout: Duration) -> bool {
            let until = Instant::now() + timeout;
            while Instant::now() < until {
                if self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(a, n)| *a == action && n.eq_ignore_ascii_case(name))
                {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            // SAFETY: `h` is the handle opened in `new`; cancelling wakes the blocked read.
            unsafe {
                CancelIoEx(self.h, std::ptr::null());
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            // SAFETY: the reader thread has ended.
            unsafe {
                CloseHandle(self.h);
            }
        }
    }
}
