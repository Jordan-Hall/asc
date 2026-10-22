//! Repository ownership and `safe.directory` (git's `ensure_valid_ownership`), the gate every
//! code path that opens a workspace passes before it reads repo-local config or runs anything
//! a repo can name. A workspace whose root, gitdir or common dir belongs to another user is
//! refused with git's own message, unless `safe.directory` (read from system, global and
//! command-line config only, never from the repo) lists it.

use std::io;
use std::path::{Path, PathBuf};

/// Probe: does the current user own `path`? An error counts as "no" to every caller.
pub type OwnerProbe<'a> = &'a dyn Fn(&Path) -> io::Result<bool>;

/// The `safe.directory` values of the protected config layers, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SafeDirectory {
    entries: Vec<String>,
}

impl SafeDirectory {
    /// Builds the list from every `safe.directory` value, lowest-precedence layer first. An
    /// empty value clears what came before, as in git.
    pub fn new<I: IntoIterator<Item = String>>(values: I) -> Self {
        let mut entries = Vec::new();
        for v in values {
            if v.is_empty() {
                entries.clear();
            } else {
                entries.push(v);
            }
        }
        Self { entries }
    }

    /// Whether some entry names `path`: `*`, an exact path, or a `<dir>/*` prefix.
    pub fn allows(&self, path: &Path) -> bool {
        let target = comparable(path);
        self.entries.iter().any(|entry| {
            if entry == "*" {
                return true;
            }
            let expanded = expand_home(entry);
            if let Some(prefix) = expanded.strip_suffix("/*").filter(|p| !p.is_empty()) {
                let prefix = comparable(Path::new(prefix));
                return target.len() > prefix.len()
                    && target.starts_with(&prefix)
                    && target.as_bytes()[prefix.len()] == b'/';
            }
            let entry_path = Path::new(&expanded);
            if comparable(entry_path) == target {
                return true;
            }
            std::fs::canonicalize(entry_path).is_ok_and(|c| comparable(&c) == target)
        })
    }
}

/// `~/rest` against the home directory, as git's `interpolate_path` does.
fn expand_home(entry: &str) -> String {
    match (entry.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.to_string_lossy().replace('\\', "/")),
        _ => entry.to_owned(),
    }
}

/// A path as a string two paths can be compared by: forward slashes, no `\\?\`, no trailing
/// slash, and lower case where the file system ignores case.
fn comparable(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        if let Some(rest) = s.strip_prefix("//?/UNC/") {
            s = format!("//{rest}");
        } else if let Some(rest) = s.strip_prefix("//?/") {
            s = rest.to_owned();
        }
        s.make_ascii_lowercase();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// `path` with the `\\?\` prefix `canonicalize` adds on Windows removed, for Win32 calls and
/// for messages.
pub fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        if let Some(rest) = s.strip_prefix("//?/UNC/") {
            return format!("//{rest}");
        }
        if let Some(rest) = s.strip_prefix("//?/") {
            return rest.to_owned();
        }
    }
    s
}

/// Quotes `s` for a shell only when it needs it (git's `sq_quote_buf_pretty`).
fn quote_pretty(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Git's refusal text, without the trailing newline, exit code 128.
pub fn dubious_message(path: &Path) -> String {
    let shown = display_path(path);
    format!(
        "fatal: detected dubious ownership in repository at '{shown}'\n\
         To add an exception for this directory, call:\n\
         \n\
         \tgit config --global --add safe.directory {}",
        quote_pretty(&shown)
    )
}

/// Succeeds when `shown` is listed in `safe`, or when the current user owns every path in
/// `paths` (each canonicalised first, so a symlink cannot borrow another owner). The error is
/// the message to print. A path that cannot be examined counts as not owned.
pub fn ensure_owned(
    shown: &Path,
    paths: &[&Path],
    safe: &SafeDirectory,
    owned: OwnerProbe<'_>,
) -> Result<(), String> {
    let canonical_shown = std::fs::canonicalize(shown).unwrap_or_else(|_| shown.to_owned());
    if safe.allows(shown) || safe.allows(&canonical_shown) {
        return Ok(());
    }
    for path in paths {
        let real: PathBuf = std::fs::canonicalize(path).map_err(|_| dubious_message(&canonical_shown))?;
        if !owned(&real).unwrap_or(false) {
            return Err(dubious_message(&canonical_shown));
        }
    }
    Ok(())
}

/// Whether the current user owns `path`: the effective uid on Unix, the owner SID on Windows.
pub fn owned_by_current_user(path: &Path) -> io::Result<bool> {
    imp::owned_by_current_user(path)
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub fn owned_by_current_user(path: &Path) -> io::Result<bool> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(meta.uid() == rustix::process::geteuid().as_raw())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 security descriptors; every call is confined to this module.
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, EqualSid, GetTokenInformation, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WinBuiltinAdministratorsSid,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// A `TOKEN_USER` buffer this module owns; the SID points into it.
    pub struct OwnedSid(Vec<u64>);

    /// A SID allocated by Win32 (`LocalFree`d on drop).
    #[cfg(test)]
    pub struct LocalSid(PSID);

    #[cfg(test)]
    impl Drop for LocalSid {
        fn drop(&mut self) {
            // SAFETY: the pointer came from a Win32 call documented to need LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }

    #[cfg(test)]
    impl LocalSid {
        pub fn as_ptr(&self) -> PSID {
            self.0
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
            if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(buf)
        })();
        // SAFETY: `token` was opened above.
        unsafe { CloseHandle(token) };
        // The buffer holds a TOKEN_USER whose SID points into the same buffer, so keep it whole.
        Ok(OwnedSid(result?))
    }

    /// The SID inside a [`current_user_sid`] buffer.
    pub fn user_sid_ptr(buf: &OwnedSid) -> PSID {
        // SAFETY: built by `current_user_sid` from a TOKEN_USER.
        unsafe { (*(buf.0.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    /// `ConvertStringSidToSidW`, for tests.
    #[cfg(test)]
    pub fn sid_from_string(text: &str) -> io::Result<LocalSid> {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut sid: PSID = null_mut();
        // SAFETY: `wide` is NUL-terminated; `sid` receives a LocalAlloc'd SID.
        if unsafe { windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalSid(sid))
    }

    /// `EqualSid`.
    pub fn sid_equal(a: PSID, b: PSID) -> bool {
        // SAFETY: callers pass valid SIDs.
        unsafe { EqualSid(a, b) != 0 }
    }

    /// Git for Windows' rule: the owner is the current user, or the Administrators group while
    /// the process is an elevated administrator (`is_admin` is that membership test).
    pub fn owner_accepted(owner: PSID, user: PSID, is_admin: impl FnOnce() -> bool) -> bool {
        if sid_equal(owner, user) {
            return true;
        }
        // SAFETY: `owner` is a valid SID.
        let owner_is_admins = unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0;
        owner_is_admins && is_admin()
    }

    /// Whether the process token enables the Administrators group (true only when elevated).
    fn token_is_elevated_admin() -> bool {
        let mut size = 68u32; // SECURITY_MAX_SID_SIZE
        let mut admins = vec![0u64; 9];
        // SAFETY: the buffer is 72 bytes, at least SECURITY_MAX_SID_SIZE.
        let made = unsafe {
            CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), admins.as_mut_ptr().cast(), &mut size)
        };
        if made == 0 {
            return false;
        }
        let mut member = 0i32;
        // SAFETY: a null token means the calling thread's effective token.
        let ok = unsafe { CheckTokenMembership(null_mut(), admins.as_ptr() as PSID, &mut member) };
        ok != 0 && member != 0
    }

    pub fn owned_by_current_user(path: &Path) -> io::Result<bool> {
        let shown = super::display_path(path).replace('/', "\\");
        let wide: Vec<u16> = Path::new(&shown).as_os_str().encode_wide().chain(Some(0)).collect();
        let me = current_user_sid()?;
        let mut owner: PSID = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: out-parameters are valid; the descriptor is freed below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let accepted = !owner.is_null() && owner_accepted(owner, user_sid_ptr(&me), token_is_elevated_admin);
        // SAFETY: allocated by GetNamedSecurityInfoW.
        unsafe { LocalFree(descriptor) };
        Ok(accepted)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    use std::path::Path;

    pub fn owned_by_current_user(_path: &Path) -> io::Result<bool> {
        Ok(false) // fail closed on a platform we cannot examine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe(values: &[&str]) -> SafeDirectory {
        SafeDirectory::new(values.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn star_exact_and_prefix_entries() {
        assert!(safe(&["*"]).allows(Path::new("/srv/repo")));
        assert!(!safe(&[]).allows(Path::new("/srv/repo")));
        assert!(safe(&["/srv/repo"]).allows(Path::new("/srv/repo/")));
        assert!(!safe(&["/srv/repo"]).allows(Path::new("/srv/repo/sub")));
        assert!(!safe(&["/srv/repo"]).allows(Path::new("/srv/repo2")));
        let prefix = safe(&["/srv/*"]);
        assert!(prefix.allows(Path::new("/srv/a")));
        assert!(prefix.allows(Path::new("/srv/a/b")));
        assert!(!prefix.allows(Path::new("/srv")));
        assert!(!prefix.allows(Path::new("/srvx/a")));
    }

    #[test]
    fn an_empty_value_resets_the_list() {
        assert!(!safe(&["*", ""]).allows(Path::new("/x")));
        assert!(safe(&["", "*"]).allows(Path::new("/x")));
    }

    #[test]
    fn the_message_is_gits() {
        let m = dubious_message(Path::new("/srv/it's"));
        let mut lines = m.lines();
        assert_eq!(
            lines.next().unwrap(),
            "fatal: detected dubious ownership in repository at '/srv/it's'"
        );
        assert_eq!(lines.next().unwrap(), "To add an exception for this directory, call:");
        assert_eq!(lines.next().unwrap(), "");
        assert_eq!(
            lines.next().unwrap(),
            "\tgit config --global --add safe.directory '/srv/it'\\''s'"
        );
        assert!(dubious_message(Path::new("/srv/repo")).ends_with("safe.directory /srv/repo"));
    }

    #[test]
    fn ensure_owned_checks_every_path_and_honours_the_list() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let only_a = |p: &Path| Ok(p.ends_with("a"));
        let none = SafeDirectory::default();
        assert!(ensure_owned(&a, &[&a], &none, &only_a).is_ok());
        let err = ensure_owned(&a, &[&a, &b], &none, &only_a).unwrap_err();
        assert!(err.starts_with("fatal: detected dubious ownership in repository at '"), "{err}");
        let broken = |_: &Path| Err(io::Error::other("no"));
        assert!(ensure_owned(&a, &[&a], &none, &broken).is_err(), "an unreadable owner fails closed");
        let allowed = safe(&[display_path(&std::fs::canonicalize(&a).unwrap()).as_str()]);
        assert!(ensure_owned(&a, &[&a, &b], &allowed, &broken).is_ok());
        assert!(ensure_owned(&a, &[&a, &b], &safe(&["*"]), &broken).is_ok());
    }

    #[test]
    fn a_directory_we_created_is_ours() {
        let t = tempfile::tempdir().unwrap();
        assert!(owned_by_current_user(t.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_of_another_uid_is_not_ours() {
        use std::os::unix::fs::MetadataExt;
        // `/` belongs to root; skip when the test itself runs as root.
        let root_owned = std::fs::metadata("/").unwrap().uid() == 0;
        if root_owned && rustix::process::geteuid().as_raw() != 0 {
            assert!(!owned_by_current_user(Path::new("/")).unwrap());
        }
    }

    #[cfg(windows)]
    #[test]
    fn sid_comparison_and_the_administrators_rule() {
        let admins = imp::sid_from_string("S-1-5-32-544").unwrap();
        let admins_again = imp::sid_from_string("S-1-5-32-544").unwrap();
        let everyone = imp::sid_from_string("S-1-1-0").unwrap();
        assert!(imp::sid_equal(admins.as_ptr(), admins_again.as_ptr()));
        assert!(!imp::sid_equal(admins.as_ptr(), everyone.as_ptr()));

        let me = imp::current_user_sid().unwrap();
        let me_ptr = imp::user_sid_ptr(&me);
        // Not the user: refused, whatever the admin state.
        assert!(!imp::owner_accepted(everyone.as_ptr(), me_ptr, || true));
        // The user's own SID is accepted without asking about elevation.
        assert!(imp::owner_accepted(me_ptr, me_ptr, || panic!("not needed")));
        // The Administrators group owns it: accepted only for an elevated administrator.
        assert!(imp::owner_accepted(admins.as_ptr(), me_ptr, || true));
        assert!(!imp::owner_accepted(admins.as_ptr(), me_ptr, || false));
    }
}
