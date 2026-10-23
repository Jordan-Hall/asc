//! The token store's key file: created readable by the server user only, and refused on load when
//! anyone else could read it.
//!
//! * Unix: mode `0600` at creation; a file with any group or other bit is refused.
//! * Windows: created with a protected DACL (no inherited entries) that allows only the current
//!   user and `SYSTEM`, and owned by the current user. A file is refused when its owner is not the
//!   current user (Administrators count only for an elevated administrator, the rule of `git`'s
//!   `safe.directory`), when its DACL is missing (everyone has access), or when any allow entry
//!   names another principal.
//! * Elsewhere: creation and loading fail, because the file cannot be protected or checked.

use std::fs::File;
use std::io;
use std::path::Path;

/// Creates `path` (never overwriting) as a file only the current user can read, and returns it
/// open for writing.
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    imp::create_private(path)
}

/// `Ok(())` when only the current user (and the system) can read `path`; else why not.
pub(crate) fn check_private(path: &Path) -> io::Result<()> {
    imp::check_private(path)
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    use std::path::Path;

    pub fn create_private(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    pub fn check_private(path: &Path) -> io::Result<()> {
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "mode {:04o} lets other users read the key; chmod 600",
                    mode & 0o7777
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 security descriptors; every call is confined to this module.
mod imp {
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CheckTokenMembership, CreateWellKnownSid,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, GetTokenInformation, INHERIT_ONLY_ACE, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
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

    /// Gives the file at `path` the owner `user` and a protected DACL allowing `user` and
    /// `SYSTEM` full access, and nobody else.
    fn lock_down(path: &Path, user: &OwnedSid) -> io::Result<()> {
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
        let (mut owner, mut dacl): (PSID, *mut ACL) = (null_mut(), null_mut());
        let (mut present, mut defaulted) = (0i32, 0i32);
        // SAFETY: `sd` is a valid absolute-or-relative descriptor; the out pointers are valid and
        // point into `sd`.
        let ok = unsafe {
            GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) != 0
                && GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) != 0
        };
        if !ok || present == 0 || dacl.is_null() || owner.is_null() {
            return Err(io::Error::other("cannot read the key file descriptor"));
        }
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated; `owner` and `dacl` live as long as `sd`.
        let status = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                owner,
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    pub fn create_private(path: &Path) -> io::Result<File> {
        let user = current_user_sid()?;
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        // The file is empty until the key is written, which happens after this returns.
        if let Err(e) = lock_down(path, &user) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        Ok(file)
    }

    /// ACE types that grant nothing (deny, audit, alarm, mandatory label and their object forms).
    fn grants_nothing(ace_type: u8) -> bool {
        matches!(ace_type, 1..=3 | 6..=8 | 10 | 12..=20)
    }

    pub fn check_private(path: &Path) -> io::Result<()> {
        let refuse = |why: &str| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{why}; recreate it with `asc serve token create`"),
            ))
        };
        let me = current_user_sid()?;
        let system = well_known(WinLocalSystemSid)?;
        let name = wide(path);
        let (mut owner, mut dacl): (PSID, *mut ACL) = (null_mut(), null_mut());
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: out-parameters are valid; the descriptor is freed below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
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
            return refuse("the key file has no owner");
        }
        // SAFETY: `owner` and `me` are valid SIDs.
        let owner_ok = unsafe { EqualSid(owner, me.as_ptr()) } != 0
            || (unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0
                && token_is_elevated_admin());
        if !owner_ok {
            return refuse("the key file is not owned by the current user");
        }
        if dacl.is_null() {
            return refuse("the key file has no access list, so everyone can read it");
        }
        // SAFETY: a non-null DACL points at a valid ACL header inside `sd`.
        let count = unsafe { (*dacl).AceCount };
        for i in 0..u32::from(count) {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `i` is below the ACE count.
            if unsafe { GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
                return refuse("the key file's access list is unreadable");
            }
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { *(ace as *const ACE_HEADER) };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || grants_nothing(header.AceType)
            {
                continue;
            }
            if header.AceType != 0 {
                return refuse("the key file's access list has an entry of an unknown kind");
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
                return refuse(&format!("the key file grants access to {who}"));
            }
        }
        Ok(())
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

    pub fn check_private(_path: &Path) -> io::Result<()> {
        Err(io::Error::other("cannot check a key file on this platform"))
    }
}
