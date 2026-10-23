//! Local IPC transport for the `asc.v1.Workspace` service (SPEC §14, "IPC").
//!
//! The daemon listens on a per-user local socket: the named pipe `\\.\pipe\asc-<user SID>` on
//! Windows (DACL: that SID and SYSTEM only), `$XDG_RUNTIME_DIR/asc/asc.sock` or
//! `$ASC_HOME/run/asc.sock` on Unix (directory 0700, socket 0600). [`IpcStream`] adapts a local
//! socket connection to tonic ([`Connected`]); [`serve_incoming`] feeds a listener to
//! `Server::serve_with_incoming`, and [`connect`] builds a [`Channel`] over the same endpoint.
//! The generated `Workspace` service traits are re-exported here.
//!
//! **Trust runs both ways.** The listener admits only this user (pipe DACL, created as the
//! first instance of its name so nobody can pre-create it; socket in a private directory with
//! an euid check per peer). The dialling side does not trust the name either: every connection
//! made by [`connect_stream`], [`connect`] or [`connect_verified`] asks the OS who the server
//! process is (`GetNamedPipeServerProcessId` plus the process token's user SID on Windows,
//! `SO_PEERCRED` euid on Unix) and is refused with [`io::ErrorKind::PermissionDenied`] unless
//! that process runs as the current user. Nothing is sent to a refused server. A pid that the
//! server only *claims* (for example in `Ping`) is never evidence of anything.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{self, BoxStream};
use hyper_util::rt::TokioIo;
use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, ToFsName, ToNsName,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub use crate::services::workspace_client::WorkspaceClient;
pub use crate::services::workspace_server::{Workspace, WorkspaceServer};

/// Why an IPC endpoint could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// Neither `XDG_RUNTIME_DIR` nor `ASC_HOME` is usable, or the user SID is unavailable.
    #[error("no per-user runtime location for the asc socket: {0}")]
    NoRuntimeDir(String),
    /// Another daemon already answers on the endpoint.
    #[error("a daemon is already listening on {0}")]
    InUse(String),
    /// The runtime directory exists but is not private to this user.
    #[error("runtime directory {0} is not private to this user")]
    UnsafeDir(String),
    /// An operating-system error.
    #[error("ipc: {0}")]
    Io(#[from] io::Error),
}

/// Where the daemon listens: a pipe name (Windows) or a socket path (Unix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcEndpoint(String);

impl IpcEndpoint {
    /// The endpoint for the current user from the process environment.
    pub fn for_current_user() -> Result<Self, IpcError> {
        #[cfg(windows)]
        {
            Ok(Self::pipe(&win::user_sid()?))
        }
        #[cfg(unix)]
        {
            Self::unix(
                std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
                std::env::var_os("ASC_HOME").as_deref(),
            )
        }
    }

    /// The pipe endpoint for a user SID string (`asc-<sid>`).
    pub fn pipe(sid: &str) -> Self {
        Self(format!("asc-{sid}"))
    }

    /// The Unix endpoint from the two environment values; `XDG_RUNTIME_DIR` wins.
    pub fn unix(
        xdg_runtime_dir: Option<&std::ffi::OsStr>,
        asc_home: Option<&std::ffi::OsStr>,
    ) -> Result<Self, IpcError> {
        let path = match (
            xdg_runtime_dir.filter(|v| !v.is_empty()),
            asc_home.filter(|v| !v.is_empty()),
        ) {
            (Some(x), _) => std::path::Path::new(x).join("asc").join("asc.sock"),
            (None, Some(h)) => std::path::Path::new(h).join("run").join("asc.sock"),
            (None, None) => {
                return Err(IpcError::NoRuntimeDir(
                    "XDG_RUNTIME_DIR and ASC_HOME unset".into(),
                ));
            }
        };
        Ok(Self(path.to_string_lossy().into_owned()))
    }

    /// An endpoint at an explicit socket path (Unix) or pipe name (Windows); used by tests.
    pub fn from_raw(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The pipe name or socket path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn name(&self) -> io::Result<Name<'_>> {
        if cfg!(windows) {
            self.0.as_str().to_ns_name::<GenericNamespaced>()
        } else {
            self.0.as_str().to_fs_name::<GenericFilePath>()
        }
    }
}

/// One accepted or dialled local connection; implements tonic's [`Connected`].
pub struct IpcStream(Stream);

impl Connected for IpcStream {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// A bound listener. Dropping it removes the Unix socket file.
pub struct IpcListener {
    inner: Listener,
    #[cfg(unix)]
    own_uid: u32,
}

impl IpcListener {
    /// Binds `endpoint` with owner-only access, clearing a stale socket on Unix.
    pub fn bind(endpoint: &IpcEndpoint) -> Result<Self, IpcError> {
        #[cfg(windows)]
        {
            use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
            let sd = win::owner_only_descriptor(&win::user_sid()?)?;
            let inner = ListenerOptions::new()
                .name(endpoint.name()?)
                .security_descriptor(sd)
                .create_tokio()?;
            Ok(Self { inner })
        }
        #[cfg(unix)]
        {
            use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
            let own_uid = unix::prepare(endpoint)?;
            let inner = ListenerOptions::new()
                .name(endpoint.name()?)
                .mode(0o600)
                .create_tokio()?;
            Ok(Self { inner, own_uid })
        }
    }

    /// Accepts the next connection (on Unix, only from this user's uid).
    pub async fn accept(&self) -> io::Result<IpcStream> {
        #[cfg(unix)]
        loop {
            let stream = self.inner.accept().await?;
            if stream.peer_creds().ok().and_then(|c| c.euid()) == Some(self.own_uid) {
                return Ok(IpcStream(stream));
            }
        }
        #[cfg(windows)]
        Ok(IpcStream(self.inner.accept().await?))
    }
}

/// Turns a listener into the incoming stream `Server::serve_with_incoming` takes.
pub fn serve_incoming(listener: IpcListener) -> BoxStream<'static, io::Result<IpcStream>> {
    Box::pin(stream::unfold(listener, |l| async move {
        let item = l.accept().await;
        Some((item, l))
    }))
}

/// Dials the endpoint once; the raw connection (no gRPC). The server is verified first: see
/// [`connect_verified`].
pub async fn connect_stream(endpoint: &IpcEndpoint) -> io::Result<IpcStream> {
    connect_verified(endpoint).await.map(|(stream, _)| stream)
}

/// Dials the endpoint and returns the connection with the OS-reported pid of the server process
/// (`None` only where the OS cannot report one; the user check has still passed).
///
/// Fails with [`io::ErrorKind::PermissionDenied`] when the server cannot be shown to run as the
/// current user (another user's process, or one that cannot be inspected: fail closed). Only a
/// pid returned here may be used to signal or kill the daemon.
pub async fn connect_verified(endpoint: &IpcEndpoint) -> io::Result<(IpcStream, Option<u32>)> {
    connect_checked(endpoint, os_verify_server).await
}

/// The pid of the process serving `endpoint`, verified as in [`connect_verified`].
pub async fn verified_server_pid(endpoint: &IpcEndpoint) -> io::Result<u32> {
    let (_, pid) = connect_verified(endpoint).await?;
    pid.ok_or_else(|| untrusted("the OS did not report the server's pid"))
}

/// Dials, then lets `verify` (the OS check in production) vet the server before the connection
/// is handed out.
async fn connect_checked(
    endpoint: &IpcEndpoint,
    verify: impl FnOnce(&Stream) -> io::Result<Option<u32>>,
) -> io::Result<(IpcStream, Option<u32>)> {
    let stream = Stream::connect(endpoint.name()?).await?;
    let pid = verify(&stream)?;
    Ok((IpcStream(stream), pid))
}

fn untrusted(why: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}

/// Asks the OS who is on the other end of `stream`; Ok only for this user's process.
fn os_verify_server(stream: &Stream) -> io::Result<Option<u32>> {
    let creds = stream.peer_creds()?;
    #[cfg(windows)]
    {
        let pid = creds
            .pid()
            .ok_or_else(|| untrusted("the OS did not report the server's pid"))?;
        win::require_current_user(pid)?;
        Ok(Some(pid))
    }
    #[cfg(unix)]
    {
        unix::require_current_user(creds.euid())?;
        // `pid_t` is signed; a pid that does not fit `u32` is no pid.
        Ok(creds.pid().and_then(|p| u32::try_from(p).ok()))
    }
}

/// A lazily connecting gRPC [`Channel`] over the endpoint.
pub fn connect(endpoint: &IpcEndpoint) -> Channel {
    let endpoint = endpoint.clone();
    // The URI is a placeholder: the connector ignores it.
    Endpoint::from_static("http://asc.ipc").connect_with_connector_lazy(service_fn(move |_: Uri| {
        let endpoint = endpoint.clone();
        async move { connect_stream(&endpoint).await.map(TokioIo::new) }
    }))
}

#[cfg(unix)]
mod unix {
    use super::{IpcEndpoint, IpcError};
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::Path;

    /// This process's effective uid.
    pub(super) fn own_euid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// Ok when a peer's euid, as the kernel reports it, is ours; anything else (including no
    /// report) is refused.
    pub(super) fn require_current_user(peer_euid: Option<u32>) -> std::io::Result<()> {
        match peer_euid {
            Some(uid) if uid == own_euid() => Ok(()),
            _ => Err(super::untrusted(
                "the server does not run as the current user",
            )),
        }
    }

    /// Creates the 0700 directory, refuses one that is not ours, and removes a stale socket.
    /// Returns this user's uid.
    pub(super) fn prepare(endpoint: &IpcEndpoint) -> Result<u32, IpcError> {
        let path = Path::new(endpoint.as_str());
        let dir = path
            .parent()
            .ok_or_else(|| IpcError::NoRuntimeDir(endpoint.as_str().into()))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let own = own_euid();
        // Never chmod or bind inside a link, or a directory that is not ours: a directory owned
        // by someone else could be swapped under us, and its owner could replace the socket.
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.uid() != own {
            return Err(IpcError::UnsafeDir(dir.display().to_string()));
        }
        if meta.mode() & 0o077 != 0 {
            // Tighten our own directory; refuse if that does not make it private.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.uid() != own || meta.mode() & 0o077 != 0 {
            return Err(IpcError::UnsafeDir(dir.display().to_string()));
        }
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_socket() && m.uid() == own => {
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => return Err(IpcError::InUse(endpoint.as_str().into())),
                    Err(_) => std::fs::remove_file(path)?,
                }
            }
            Ok(_) => return Err(IpcError::UnsafeDir(path.display().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(own)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use super::IpcError;
    use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt as _;
    use interprocess::os::windows::security_descriptor::{
        BorrowedSecurityDescriptor, SecurityDescriptor,
    };
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::{Path, PathBuf};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSECURITY_DESCRIPTOR,
        PSID, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        TOKEN_USER, TokenIntegrityLevel, TokenIsAppContainer, TokenLogonSid, TokenRestrictedSids,
        TokenUser,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn os(e: windows::core::Error) -> IpcError {
        IpcError::Io(std::io::Error::other(e))
    }

    /// What the server check looks at in a process token.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct TokenFacts {
        /// The user's string SID.
        pub sid: String,
        /// The logon session's SID (`TokenLogonSid`).
        pub logon_sid: Option<String>,
        /// The integrity level's RID (`SECURITY_MANDATORY_*_RID`: 0x1000 low, 0x2000 medium,
        /// 0x3000 high, 0x4000 system).
        pub integrity: u32,
        /// `TokenIsAppContainer`.
        pub app_container: bool,
        /// The token has restricting SIDs (`IsTokenRestricted`).
        pub restricted: bool,
    }

    /// The string form of `sid`.
    ///
    /// # Safety
    /// `sid` must point to a valid SID.
    unsafe fn sid_string(sid: PSID) -> Result<String, IpcError> {
        // SAFETY: the caller vouches for the SID; the converted string is freed.
        unsafe {
            let mut out = PWSTR::null();
            ConvertSidToStringSidW(sid, &mut out).map_err(os)?;
            let s = out
                .to_string()
                .map_err(|e| IpcError::NoRuntimeDir(e.to_string()));
            let _ = LocalFree(Some(HLOCAL(out.0.cast())));
            s
        }
    }

    /// The raw bytes of one token information class, aligned for the structs they hold.
    ///
    /// # Safety
    /// `token` must be a valid token handle opened with `TOKEN_QUERY`.
    unsafe fn token_buf(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
    ) -> Result<Vec<u64>, IpcError> {
        // SAFETY: the caller vouches for the handle; the buffer is sized by the first call.
        unsafe {
            let mut len = 0u32;
            let _ = GetTokenInformation(token, class, None, 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            GetTokenInformation(
                token,
                class,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                len,
                &mut len,
            )
            .map_err(os)?;
            Ok(buf)
        }
    }

    /// The facts about `token` that the server check needs.
    ///
    /// # Safety
    /// `token` must be a valid token handle opened with `TOKEN_QUERY`.
    unsafe fn token_facts(token: HANDLE) -> Result<TokenFacts, IpcError> {
        // SAFETY: the caller vouches for the handle; each buffer holds the struct its class
        // names, and every allocation is freed.
        unsafe {
            let buf = token_buf(token, TokenUser)?;
            let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
            let sid = sid_string(user.User.Sid)?;

            let buf = token_buf(token, TokenIntegrityLevel)?;
            let label = &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            if count == 0 {
                return Err(IpcError::NoRuntimeDir(
                    "token integrity SID is empty".into(),
                ));
            }
            let integrity = *GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1);

            let buf = token_buf(token, TokenIsAppContainer)?;
            let app_container = *buf.as_ptr().cast::<u32>() != 0;

            // The logon SID names the logon session: a process of the same user in another
            // session is not the daemon this session started.
            let buf = token_buf(token, TokenLogonSid)?;
            let groups = &*(buf.as_ptr().cast::<TOKEN_GROUPS>());
            let logon_sid = if groups.GroupCount >= 1 {
                Some(sid_string(groups.Groups[0].Sid)?)
            } else {
                None
            };

            let buf = token_buf(token, TokenRestrictedSids)?;
            let restricted = (*(buf.as_ptr().cast::<TOKEN_GROUPS>())).GroupCount != 0;
            Ok(TokenFacts {
                sid,
                logon_sid,
                integrity,
                app_container,
                restricted,
            })
        }
    }

    /// The facts about the token of `process` (opened with `PROCESS_QUERY_LIMITED_INFORMATION`).
    fn process_token_facts(process: HANDLE) -> Result<TokenFacts, IpcError> {
        // SAFETY: plain Win32 calls; the token handle is closed on every path.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(os)?;
            let facts = token_facts(token);
            let _ = CloseHandle(token);
            facts
        }
    }

    /// The facts about this process's token.
    pub(super) fn current_facts() -> Result<TokenFacts, IpcError> {
        // SAFETY: the pseudo handle of the current process needs no closing.
        process_token_facts(unsafe { GetCurrentProcess() })
    }

    /// The string SID of the process token's user.
    pub(super) fn user_sid() -> Result<String, IpcError> {
        current_facts().map(|f| f.sid)
    }

    /// The facts about the token of process `pid`, and the process's image path when it can be
    /// read. Fails when the process is gone or cannot be inspected (another user's process
    /// normally cannot be opened at all).
    pub(super) fn process_facts(pid: u32) -> Result<(TokenFacts, Option<PathBuf>), IpcError> {
        // SAFETY: plain Win32 calls; the process handle is closed on every path.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(os)?;
            let facts = process_token_facts(process);
            let mut buf = vec![0u16; 4096];
            let mut len = buf.len() as u32;
            let path = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .ok()
            .map(|()| PathBuf::from(OsString::from_wide(&buf[..len as usize])));
            let _ = CloseHandle(process);
            Ok((facts?, path))
        }
    }

    /// Whether a server running with `theirs` may be trusted by a client running with `ours`:
    /// the same user in the same logon session, an integrity level at least ours (a low-integrity or sandboxed process
    /// of the same user must not be able to pose as the daemon), not an AppContainer and not
    /// a restricted token. The error says which rule failed.
    pub(super) fn judge(ours: &TokenFacts, theirs: &TokenFacts) -> Result<(), String> {
        if theirs.sid != ours.sid {
            return Err("it does not run as the current user".into());
        }
        if theirs.logon_sid != ours.logon_sid {
            return Err("it runs in another logon session".into());
        }
        if theirs.integrity < ours.integrity {
            return Err(format!(
                "its integrity level {:#x} is below ours {:#x}",
                theirs.integrity, ours.integrity
            ));
        }
        if theirs.app_container {
            return Err("it runs in an AppContainer".into());
        }
        if theirs.restricted {
            return Err("it runs with a restricted token".into());
        }
        Ok(())
    }

    /// Ok only when process `pid` may be trusted as this user's daemon (see [`judge`]; fail
    /// closed: any error is a refusal). The image path is only a signal: a server that is not
    /// this executable is logged and still accepted, because a daemon of another version
    /// (which `asc` must replace) is normally a different file.
    pub(super) fn require_current_user(pid: u32) -> std::io::Result<()> {
        let ours = current_facts().map_err(|e| refuse(pid, e.to_string()))?;
        let (theirs, path) = process_facts(pid).map_err(|e| refuse(pid, e.to_string()))?;
        judge(&ours, &theirs).map_err(|why| refuse(pid, why))?;
        note_image(pid, path.as_deref());
        Ok(())
    }

    /// Logs a server image that is not this executable.
    fn note_image(pid: u32, path: Option<&Path>) {
        let Ok(me) = std::env::current_exe() else {
            return;
        };
        match path {
            Some(p) if p == me => {}
            Some(p) if p.as_os_str().eq_ignore_ascii_case(me.as_os_str()) => {
                tracing::info!(pid, server = %p.display(), "ipc server image differs from this executable only by case");
            }
            Some(p) => {
                tracing::debug!(pid, server = %p.display(), exe = %me.display(), "ipc server runs another executable");
            }
            None => tracing::debug!(pid, "ipc server image path unavailable"),
        }
    }

    fn refuse(pid: u32, why: String) -> std::io::Error {
        super::untrusted(format!("server process {pid}: {why}"))
    }

    /// A child process of ours running at low integrity, ended on drop.
    #[cfg(test)]
    pub(super) struct LowChild {
        process: HANDLE,
        pub pid: u32,
    }

    #[cfg(test)]
    impl Drop for LowChild {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by CreateProcessAsUserW and is closed once.
            unsafe {
                let _ = windows::Win32::System::Threading::TerminateProcess(self.process, 1);
                let _ = CloseHandle(self.process);
            }
        }
    }

    /// Starts `ping` with a copy of this process's token whose integrity level is lowered to
    /// low (`S-1-16-4096`): what a sandboxed same-user process looks like.
    #[cfg(test)]
    pub(super) fn spawn_low_integrity() -> Result<LowChild, IpcError> {
        use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows::Win32::Security::{
            DuplicateTokenEx, GetLengthSid, SID_AND_ATTRIBUTES, SecurityImpersonation,
            SetTokenInformation, TOKEN_ACCESS_MASK, TOKEN_DUPLICATE, TokenPrimary,
        };
        use windows::Win32::System::Threading::{
            CREATE_NO_WINDOW, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
        };
        const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
        const SE_GROUP_INTEGRITY: u32 = 0x20;
        let sid = wide("S-1-16-4096");
        let mut cmd = wide("ping -n 60 127.0.0.1");
        // SAFETY: plain Win32 calls with valid, NUL-terminated inputs; every handle and the
        // converted SID are released on every path.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut token,
            )
            .map_err(os)?;
            let mut low = HANDLE::default();
            let dup = DuplicateTokenEx(
                token,
                TOKEN_ACCESS_MASK(MAXIMUM_ALLOWED),
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut low,
            );
            let _ = CloseHandle(token);
            dup.map_err(os)?;

            let mut psid = PSID::default();
            let made = ConvertStringSidToSidW(PCWSTR(sid.as_ptr()), &mut psid).map_err(os);
            let spawned = made.and_then(|()| {
                let label = TOKEN_MANDATORY_LABEL {
                    Label: SID_AND_ATTRIBUTES {
                        Sid: psid,
                        Attributes: SE_GROUP_INTEGRITY,
                    },
                };
                let size = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(psid);
                SetTokenInformation(
                    low,
                    TokenIntegrityLevel,
                    (&raw const label).cast::<c_void>(),
                    size,
                )
                .map_err(os)?;
                let si = STARTUPINFOW {
                    cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                    ..Default::default()
                };
                let mut pi = PROCESS_INFORMATION::default();
                CreateProcessAsUserW(
                    Some(low),
                    PCWSTR::null(),
                    Some(PWSTR(cmd.as_mut_ptr())),
                    None,
                    None,
                    false,
                    CREATE_NO_WINDOW,
                    None,
                    PCWSTR::null(),
                    &si,
                    &mut pi,
                )
                .map_err(os)?;
                let _ = CloseHandle(pi.hThread);
                Ok(LowChild {
                    process: pi.hProcess,
                    pid: pi.dwProcessId,
                })
            });
            if !psid.0.is_null() {
                let _ = LocalFree(Some(HLOCAL(psid.0)));
            }
            let _ = CloseHandle(low);
            spawned
        }
    }

    /// The SDDL granting full access to `sid` and SYSTEM only, with inheritance blocked.
    pub(super) fn owner_only_sddl(sid: &str) -> String {
        format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
    }

    /// The pipe security descriptor for `sid`.
    pub(super) fn owner_only_descriptor(sid: &str) -> Result<SecurityDescriptor, IpcError> {
        let sddl = wide(&owner_only_sddl(sid));
        // SAFETY: the SDDL is NUL-terminated; the converted descriptor is cloned into an owned
        // one and then freed.
        unsafe {
            let mut psd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
            .map_err(os)?;
            let owned = BorrowedSecurityDescriptor::from_ptr(psd.0.cast_const()).to_owned_sd();
            let _ = LocalFree(Some(HLOCAL(psd.0)));
            Ok(owned?)
        }
    }

    /// Reads the DACL of the pipe `name` back as an SDDL string.
    #[cfg(test)]
    pub(super) fn pipe_dacl_sddl(name: &str) -> Result<String, IpcError> {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows::Win32::Security::DACL_SECURITY_INFORMATION;
        let pipe = std::fs::File::open(format!(r"\\.\pipe\{name}"))?;
        // SAFETY: out-pointers are valid; the returned descriptor and string are freed.
        unsafe {
            let mut psd = PSECURITY_DESCRIPTOR::default();
            let rc = GetSecurityInfo(
                HANDLE(pipe.as_raw_handle()),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut psd),
            );
            if rc.0 != 0 {
                return Err(IpcError::Io(std::io::Error::from_raw_os_error(rc.0 as i32)));
            }
            let mut out = PWSTR::null();
            let r = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                psd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut out,
                None,
            );
            let s = r.map(|()| out.to_string().unwrap_or_default());
            let _ = LocalFree(Some(HLOCAL(out.0.cast())));
            let _ = LocalFree(Some(HLOCAL(psd.0)));
            s.map_err(os)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tonic::codegen::http::{Request, Response};
    use tonic::server::NamedService;

    fn unique() -> String {
        format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple())
    }

    fn test_endpoint(dir: &std::path::Path) -> IpcEndpoint {
        if cfg!(windows) {
            IpcEndpoint::from_raw(format!("asc-test-{}", unique()))
        } else {
            IpcEndpoint::from_raw(
                dir.join("asc")
                    .join("asc.sock")
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    #[test]
    fn unix_paths_prefer_xdg_then_asc_home() {
        use std::ffi::OsStr;
        let x = IpcEndpoint::unix(Some(OsStr::new("/run/u")), Some(OsStr::new("/h"))).unwrap();
        assert_eq!(x.as_str().replace('\\', "/"), "/run/u/asc/asc.sock");
        let h = IpcEndpoint::unix(None, Some(OsStr::new("/h"))).unwrap();
        assert_eq!(h.as_str().replace('\\', "/"), "/h/run/asc.sock");
        let e = IpcEndpoint::unix(Some(OsStr::new("")), None);
        assert!(matches!(e, Err(IpcError::NoRuntimeDir(_))));
    }

    #[test]
    fn pipe_name_embeds_sid() {
        assert_eq!(IpcEndpoint::pipe("S-1-5-21-1").as_str(), "asc-S-1-5-21-1");
    }

    #[tokio::test]
    async fn same_user_connects_and_exchanges_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ep = test_endpoint(dir.path());
        let listener = IpcListener::bind(&ep).unwrap();
        let server = tokio::spawn(async move {
            let mut s = listener.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
            s.flush().await.unwrap();
            // Keep the listener alive until the client has read the echo.
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
        });
        let mut c = connect_stream(&ep).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        c.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
        drop(c);
        server.await.unwrap();
    }

    /// A stand-in `asc.v1.Workspace` that answers every call with `UNIMPLEMENTED`.
    #[derive(Clone)]
    struct Stub;

    impl NamedService for Stub {
        const NAME: &'static str = "asc.v1.Workspace";
    }

    impl tower::Service<Request<tonic::body::Body>> for Stub {
        type Response = Response<tonic::body::Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: Request<tonic::body::Body>) -> Self::Future {
            std::future::ready(Ok(tonic::Status::unimplemented("stub").into_http()))
        }
    }

    #[tokio::test]
    async fn grpc_client_reaches_server_over_ipc() {
        let dir = tempfile::tempdir().unwrap();
        let ep = test_endpoint(dir.path());
        let incoming = serve_incoming(IpcListener::bind(&ep).unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(Stub)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = rx.await;
                })
                .await
        });
        let mut client = WorkspaceClient::new(connect(&ep));
        let err = client
            .ping(crate::pb::PingRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert_eq!(err.message(), "stub");
        let _ = tx.send(());
        let _ = server.await;
    }

    #[test]
    fn missing_endpoint_fails_to_connect() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ep = IpcEndpoint::from_raw(if cfg!(windows) {
            format!("asc-absent-{}", unique())
        } else {
            "/nonexistent/asc.sock".into()
        });
        assert!(rt.block_on(connect_stream(&ep)).is_err());
    }

    #[tokio::test]
    async fn the_verified_server_pid_is_the_os_reported_one() {
        let dir = tempfile::tempdir().unwrap();
        let ep = test_endpoint(dir.path());
        let _listener = IpcListener::bind(&ep).unwrap();
        // The listener lives in this process, so the OS must name this process.
        assert_eq!(verified_server_pid(&ep).await.unwrap(), std::process::id());
    }

    #[tokio::test]
    async fn a_server_that_fails_verification_is_refused_and_gets_no_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ep = test_endpoint(dir.path());
        let listener = IpcListener::bind(&ep).unwrap();
        let server = tokio::spawn(async move {
            let mut s = listener.accept().await.unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
            rest
        });
        let refused = connect_checked(&ep, |_| Err(untrusted("not our user"))).await;
        assert_eq!(
            refused.err().map(|e| e.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );
        // The refused connection was dropped before anything was written to it.
        assert!(server.await.unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_user_check_accepts_our_processes_and_refuses_the_rest() {
        win::require_current_user(std::process::id()).unwrap();
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ours = win::require_current_user(child.id());
        let _ = child.kill();
        let _ = child.wait();
        ours.unwrap();
        // pid 4 is the System process: another user, and not openable without privilege.
        let system = win::require_current_user(4).unwrap_err();
        assert_eq!(system.kind(), io::ErrorKind::PermissionDenied, "{system}");
        // A pid nobody holds is refused too.
        let gone = win::require_current_user(0x7fff_fff0).unwrap_err();
        assert_eq!(gone.kind(), io::ErrorKind::PermissionDenied, "{gone}");
    }

    #[cfg(windows)]
    fn facts() -> win::TokenFacts {
        win::TokenFacts {
            sid: "S-1-5-21-1-2-3-1001".into(),
            logon_sid: Some("S-1-5-5-0-111".into()),
            integrity: 0x2000,
            app_container: false,
            restricted: false,
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_server_token_rules() {
        let ours = facts();
        let refused = |theirs: win::TokenFacts| win::judge(&ours, &theirs).unwrap_err();
        win::judge(&ours, &ours).unwrap();
        win::judge(
            &ours,
            &win::TokenFacts {
                integrity: 0x3000,
                ..facts()
            },
        )
        .unwrap();

        let other_user = refused(win::TokenFacts {
            sid: "S-1-5-18".into(),
            ..facts()
        });
        assert!(other_user.contains("current user"), "{other_user}");
        let other_session = refused(win::TokenFacts {
            logon_sid: Some("S-1-5-5-0-222".into()),
            ..facts()
        });
        assert!(other_session.contains("logon session"), "{other_session}");
        let no_session = refused(win::TokenFacts {
            logon_sid: None,
            ..facts()
        });
        assert!(no_session.contains("logon session"), "{no_session}");
        let low = refused(win::TokenFacts {
            integrity: 0x1000,
            ..facts()
        });
        assert!(low.contains("integrity"), "{low}");
        let sandbox = refused(win::TokenFacts {
            app_container: true,
            ..facts()
        });
        assert!(sandbox.contains("AppContainer"), "{sandbox}");
        let restricted = refused(win::TokenFacts {
            restricted: true,
            ..facts()
        });
        assert!(restricted.contains("restricted"), "{restricted}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_our_own_token_is_a_trusted_server() {
        let me = win::current_facts().unwrap();
        assert!(
            me.integrity >= 0x2000,
            "tests run at medium integrity or above: {me:?}"
        );
        assert!(!me.app_container && !me.restricted, "{me:?}");
        assert!(me.logon_sid.is_some(), "{me:?}");
        let (seen, image) = win::process_facts(std::process::id()).unwrap();
        assert_eq!(seen, me);
        assert_eq!(image.as_deref(), std::env::current_exe().ok().as_deref());
        win::require_current_user(std::process::id()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_a_low_integrity_process_of_ours_is_refused() {
        let low = win::spawn_low_integrity().expect("start a low-integrity child");
        // It is the same user and session, so only the integrity rule can refuse it.
        let (theirs, _) = win::process_facts(low.pid).unwrap();
        let ours = win::current_facts().unwrap();
        assert_eq!(theirs.sid, ours.sid);
        assert_eq!(theirs.logon_sid, ours.logon_sid);
        assert_eq!(theirs.integrity, 0x1000, "{theirs:?}");
        let refused = win::require_current_user(low.pid).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
        assert!(refused.to_string().contains("integrity"), "{refused}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_pipe_name_cannot_be_taken_by_another_creator() {
        use tokio::net::windows::named_pipe::ServerOptions;
        let dir = tempfile::tempdir().unwrap();

        // Our listener holds the name: a second daemon cannot bind it.
        let ep = test_endpoint(dir.path());
        let _first = IpcListener::bind(&ep).unwrap();
        assert!(IpcListener::bind(&ep).is_err());

        // A squatter that created the pipe first (an ordinary server with the default DACL)
        // keeps the name: our bind must not join it as one more instance. That needs
        // FILE_FLAG_FIRST_PIPE_INSTANCE on our first instance.
        let squatted = test_endpoint(dir.path());
        let _squatter = ServerOptions::new()
            .create(format!(r"\\.\pipe\{}", squatted.as_str()))
            .unwrap();
        assert!(
            IpcListener::bind(&squatted).is_err(),
            "bind joined a pipe that someone else created"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_peer_check_accepts_only_our_euid() {
        let own = unix::own_euid();
        unix::require_current_user(Some(own)).unwrap();
        for other in [Some(own.wrapping_add(1)), None] {
            let e = unix::require_current_user(other).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_run_dir_that_is_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("asc")).unwrap();
        let ep = test_endpoint(dir.path());
        assert!(matches!(
            IpcListener::bind(&ep),
            Err(IpcError::UnsafeDir(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn pipe_dacl_allows_only_user_and_system() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let ep = test_endpoint(dir.path());
        let _listener = IpcListener::bind(&ep).unwrap();
        let sddl = win::pipe_dacl_sddl(ep.as_str()).unwrap();
        let sid = win::user_sid().unwrap();
        assert!(sddl.starts_with("D:P"), "protected DACL expected: {sddl}");
        assert_eq!(sddl.matches("(A;").count(), 2, "exactly two ACEs: {sddl}");
        assert_eq!(sddl.matches("(D;").count(), 0, "{sddl}");
        assert!(sddl.contains(&sid) || sddl.contains(";;;SY)"), "{sddl}");
        assert!(sddl.contains(";;;SY)"), "SYSTEM ace: {sddl}");
        assert!(
            sddl.contains(&format!(";;;{sid})")) || sddl.contains(";;;OW)"),
            "user ace: {sddl}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_is_private_and_stale_socket_is_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ep = test_endpoint(dir.path());
        {
            let _l = IpcListener::bind(&ep).unwrap();
            let p = std::path::Path::new(ep.as_str());
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(p.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert!(matches!(
                IpcListener::bind(&ep),
                Err(IpcError::InUse(_)) | Err(IpcError::Io(_))
            ));
        }
        // A leftover socket file with no listener is cleared by the next bind.
        let _l = IpcListener::bind(&ep).unwrap();
    }
}
