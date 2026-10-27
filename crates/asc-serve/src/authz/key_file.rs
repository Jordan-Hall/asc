//! The token store's key file: created readable by the server user only, and refused on load when
//! anyone else could read it.
//!
//! Both steps close their race windows. A file is created already protected (no moment where it
//! exists with a looser list), and it is checked through the handle that is then read from, never
//! through its path (no moment where another file can be swapped in between check and use).
//!
//! * Unix: created with `O_CREAT|O_EXCL` and mode `0600`. Opened only if the path is not a symbolic
//!   link and names the same file the handle refers to; the handle's mode must have no group or
//!   other bit.
//! * Windows: created by `CreateFileW(CREATE_NEW)` with a security descriptor that owns the file
//!   as the current user and carries a protected DACL (no inherited entries) allowing only the
//!   current user and `SYSTEM`. Opened with `FILE_FLAG_OPEN_REPARSE_POINT`, so a link is opened
//!   as itself and refused. The handle's owner must be the current user (Administrators count only
//!   for an elevated administrator, the rule of `git`'s `safe.directory`), its DACL must exist
//!   (a missing one means everyone has access), and every allow entry must name the current user
//!   or `SYSTEM`.
//! * Elsewhere: creation and loading fail, because the file cannot be protected or checked.

use std::fs::File;
use std::io;
use std::path::Path;

/// Creates `path` (never overwriting) as a file only the current user can read, protected from
/// the first instant it exists, and returns it open for writing.
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    imp::create_private(path)
}

/// Opens `path` for reading and checks the opened file: `Ok` only when no one but the current
/// user (and the system) can read it.
pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    imp::open_private(path)
}

/// Checks an already opened key file by its handle: `Ok` only when no one but the current user
/// (and the system) can read it. For a caller that opened the file itself.
#[cfg(feature = "git-ssh")]
pub(crate) fn check_private(file: &File) -> io::Result<()> {
    imp::check_private(file)
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::path::Path;

    fn denied(msg: String) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, msg)
    }

    pub fn create_private(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    pub fn open_private(path: &Path) -> io::Result<File> {
        let link = std::fs::symlink_metadata(path)?;
        if link.file_type().is_symlink() {
            return Err(denied("the key file is a symbolic link".into()));
        }
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if (meta.dev(), meta.ino()) != (link.dev(), link.ino()) {
            return Err(denied("the key file changed while it was opened".into()));
        }
        check_private(&file)?;
        Ok(file)
    }

    pub fn check_private(file: &File) -> io::Result<()> {
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(denied("the key file is not a regular file".into()));
        }
        check_owner(meta.uid(), rustix::process::getuid().as_raw())?;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(denied(format!(
                "mode {:04o} lets other users read the key; chmod 600",
                mode & 0o7777
            )));
        }
        Ok(())
    }

    /// The key file belongs to the user running the server: a file another user owns could be
    /// replaced or re-moded by them whatever its mode is now.
    pub fn check_owner(file_uid: u32, process_uid: u32) -> io::Result<()> {
        if file_uid == process_uid {
            Ok(())
        } else {
            Err(denied(format!(
                "the key file is owned by uid {file_uid}, not by this process (uid {process_uid})"
            )))
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        #[test]
        fn created_private_and_checked_on_the_handle() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("k");
            let file = create_private(&path).unwrap();
            check_private(&file).unwrap();
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            drop(file);
            open_private(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(open_private(&path).is_err());
            let handle = File::open(&path).unwrap();
            assert!(check_private(&handle).is_err());
        }

        #[test]
        fn a_file_of_another_owner_is_refused() {
            let me = rustix::process::getuid().as_raw();
            check_owner(me, me).unwrap();
            let e = check_owner(me.wrapping_add(1), me).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert!(e.to_string().contains("owned by uid"), "{e}");
        }

        #[test]
        fn a_symbolic_link_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let real = dir.path().join("real");
            drop(create_private(&real).unwrap());
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(open_private(&link).is_err());
            open_private(&real).unwrap();
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 security descriptors; every call is confined to this module.
mod imp {
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CheckTokenMembership, CreateWellKnownSid,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, INHERIT_ONLY_ACE,
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
        WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn denied(msg: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{msg}; fix its permissions or recreate it"),
        )
    }

    /// A `TOKEN_USER` buffer; the SID points into it.
    pub struct OwnedSid(Vec<u64>);

    impl OwnedSid {
        pub fn as_ptr(&self) -> PSID {
            // SAFETY: built by `current_user_sid` from a TOKEN_USER.
            unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
        }
    }

    /// The SID of the user the process runs as.
    pub fn current_user_sid() -> io::Result<OwnedSid> {
        let mut token: HANDLE = null_mut();
        // SAFETY: plain out-parameter call; the handle is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut len = 0u32;
            // SAFETY: a null buffer with length 0 asks for the size.
            unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            // SAFETY: `buf` holds at least `len` bytes.
            if unsafe {
                GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(buf)
        })();
        // SAFETY: `token` was opened above.
        unsafe { CloseHandle(token) };
        Ok(OwnedSid(result?))
    }

    fn well_known(kind: i32) -> io::Result<Vec<u64>> {
        let mut size = 68u32; // SECURITY_MAX_SID_SIZE
        let mut sid = vec![0u64; 9];
        // SAFETY: the buffer is 72 bytes, at least SECURITY_MAX_SID_SIZE.
        let made =
            unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr().cast(), &mut size) };
        if made == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sid)
    }

    /// Whether the process token enables the Administrators group (true only when elevated).
    fn token_is_elevated_admin() -> bool {
        let Ok(admins) = well_known(WinBuiltinAdministratorsSid) else {
            return false;
        };
        let mut member = 0i32;
        // SAFETY: a null token means the calling thread's effective token.
        let ok = unsafe { CheckTokenMembership(null_mut(), admins.as_ptr() as PSID, &mut member) };
        ok != 0 && member != 0
    }

    fn sid_string(sid: PSID) -> io::Result<String> {
        let mut text: *mut u16 = null_mut();
        // SAFETY: `text` receives a LocalAlloc'd, NUL-terminated string.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        // SAFETY: the string is NUL-terminated.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` code units were just measured.
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(text.cast()) };
        Ok(s)
    }

    /// Frees a Win32 allocation on drop.
    struct Local(*mut c_void);

    impl Drop for Local {
        fn drop(&mut self) {
            // SAFETY: the pointer came from a Win32 call documented to need LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }

    /// Creates `path` with `CREATE_NEW`, so it never overwrites, and with the descriptor applied by
    /// the create itself: owned by the current user, a protected DACL allowing the current user
    /// and `SYSTEM` full access and nobody else.
    pub fn create_private(path: &Path) -> io::Result<File> {
        let user = current_user_sid()?;
        let sid = sid_string(user.as_ptr())?;
        let sddl = format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;SY)");
        let wsddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `wsddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wsddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let _free = Local(sd);
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `name` is NUL-terminated and `attrs` points at a valid descriptor that outlives
        // the call.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ,
                &attrs,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a valid, owned file handle that nothing else closes.
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    pub fn open_private(path: &Path) -> io::Result<File> {
        // A link is opened as the link itself, and then refused by `check_private`.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        check_private(&file)?;
        Ok(file)
    }

    /// ACE types that grant nothing (deny, audit, alarm, mandatory label and their object forms).
    fn grants_nothing(ace_type: u8) -> bool {
        matches!(ace_type, 1..=3 | 6..=8 | 10 | 12..=20)
    }

    pub fn check_private(file: &File) -> io::Result<()> {
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(denied("the key file is a link"));
        }
        let me = current_user_sid()?;
        let system = well_known(WinLocalSystemSid)?;
        let (mut owner, mut dacl): (PSID, *mut ACL) = (null_mut(), null_mut());
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the handle is open for the life of `file`; out-parameters are valid and the
        // descriptor is freed below.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut sd,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _free = Local(sd);
        if owner.is_null() {
            return Err(denied("the key file has no owner"));
        }
        // SAFETY: `owner` and `me` are valid SIDs.
        let owner_ok = unsafe { EqualSid(owner, me.as_ptr()) } != 0
            || (unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0
                && token_is_elevated_admin());
        if !owner_ok {
            return Err(denied("the key file is not owned by the current user"));
        }
        if dacl.is_null() {
            return Err(denied(
                "the key file has no access list, so everyone can read it",
            ));
        }
        // SAFETY: a non-null DACL points at a valid ACL header inside `sd`.
        let count = unsafe { (*dacl).AceCount };
        for i in 0..u32::from(count) {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `i` is below the ACE count.
            if unsafe { GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
                return Err(denied("the key file's access list is unreadable"));
            }
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { *(ace as *const ACE_HEADER) };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || grants_nothing(header.AceType)
            {
                continue;
            }
            if header.AceType != 0 {
                return Err(denied(
                    "the key file's access list has an entry of an unknown kind",
                ));
            }
            // SAFETY: an ACCESS_ALLOWED_ACE (type 0) holds its SID from `SidStart`.
            let sid = unsafe { std::ptr::addr_of!((*(ace as *const ACCESS_ALLOWED_ACE)).SidStart) }
                as PSID;
            // SAFETY: both SIDs are valid.
            let ours = unsafe {
                EqualSid(sid, me.as_ptr()) != 0 || EqualSid(sid, system.as_ptr() as PSID) != 0
            };
            if !ours {
                let who = sid_string(sid).unwrap_or_else(|_| "another principal".into());
                return Err(denied(&format!("the key file grants access to {who}")));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::process::Command;

        use super::*;

        fn icacls(path: &Path, args: &[&str]) -> bool {
            Command::new("icacls")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        }

        /// The file has its protected list from the moment `create_private` returns: nothing was
        /// written yet, and the directory's own (looser) list was never applied.
        #[test]
        fn a_new_file_has_the_protected_list_immediately() {
            let dir = tempfile::tempdir().unwrap();
            // A directory whose list would be inherited by a plain new file.
            assert!(icacls(dir.path(), &["/grant", "*S-1-1-0:(OI)(CI)R"]));
            let path = dir.path().join("k");
            let file = create_private(&path).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
            check_private(&file).unwrap();
            let shown = Command::new("icacls").arg(&path).output().unwrap();
            let shown = String::from_utf8_lossy(&shown.stdout).into_owned();
            assert!(!shown.contains("Everyone"), "{shown}");
            assert!(!shown.contains("(I)"), "{shown}");
            // A plain file in the same directory does inherit the loose list.
            let plain = dir.path().join("plain");
            let plain_file = std::fs::File::create(&plain).unwrap();
            assert!(check_private(&plain_file).is_err());
            // Creation never overwrites.
            drop(file);
            assert_eq!(
                create_private(&path).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
        }

        /// The check reads the opened handle: a path swapped afterwards does not change what the
        /// handle says, and a loose file is refused through its own handle.
        #[test]
        fn the_check_is_on_the_handle() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("k");
            drop(create_private(&path).unwrap());
            let good = open_private(&path).unwrap();

            // Swap a world-readable file in at the same path.
            std::fs::rename(&path, dir.path().join("k.moved")).unwrap();
            std::fs::write(&path, "x").unwrap();
            assert!(icacls(&path, &["/grant", "*S-1-1-0:R"]));
            assert!(open_private(&path).is_err());
            // The handle opened earlier is still the protected file.
            check_private(&good).unwrap();
            let loose = File::open(&path).unwrap();
            assert!(check_private(&loose).is_err());

            // A world-readable file is refused after the fact too.
            let again = dir.path().join("again");
            drop(create_private(&again).unwrap());
            let handle = open_private(&again).unwrap();
            assert!(icacls(&again, &["/grant", "*S-1-1-0:R"]));
            assert!(check_private(&handle).is_err());
        }

        #[test]
        fn a_link_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let real = dir.path().join("real");
            drop(create_private(&real).unwrap());
            let link = dir.path().join("link");
            // Creating a symbolic link needs a privilege; skip where it is missing.
            if std::os::windows::fs::symlink_file(&real, &link).is_err() {
                return;
            }
            assert!(open_private(&link).is_err());
            open_private(&real).unwrap();
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub fn create_private(_path: &Path) -> io::Result<File> {
        Err(io::Error::other(
            "cannot protect a key file on this platform",
        ))
    }

    pub fn open_private(_path: &Path) -> io::Result<File> {
        Err(io::Error::other("cannot check a key file on this platform"))
    }

    pub fn check_private(_file: &File) -> io::Result<()> {
        Err(io::Error::other("cannot check a key file on this platform"))
    }
}
