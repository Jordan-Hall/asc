//! `asc mcp` over stdio (SPEC 22.5, 22.7.2; unit M09).
//!
//! The protocol layer lives in `asc-mcp`; this module supplies what needs the `asc` crate: the
//! [`LocalBackend`](backend::LocalBackend) (routing, child verbs), the [path gate](pathgate) and
//! the **protocol channel guard**.
//!
//! **Protocol channel.** Standard output carries JSON-RPC and nothing else. Before anything else
//! runs, [`guard::take_stdout`] keeps a private duplicate of the real standard output for the
//! transport and points file descriptor 1 (on Windows the standard output handle) at standard
//! error. A stray print from a library, a hook or an inherited child then lands on stderr and
//! cannot corrupt the stream. Children the server starts get a null stdin and piped stdout.

pub mod backend;
pub mod pathgate;

use std::collections::BTreeSet;
use std::process::ExitCode;

use asc_mcp::config::{Entry, Flags, Layer, McpConfig};
use asc_mcp::cursor::KeyRing;
use asc_mcp::handler::{Caller, Handler, HandlerConfig};
use asc_mcp::schema::Surface;
use tokio::io::BufReader;

use crate::cli::common::{CliError, Ctx, Result};

/// The real standard output, kept for the transport.
mod guard {
    use std::fs::File;
    use std::io;

    /// Moves the process's standard output to standard error and returns the real one.
    ///
    /// # Errors
    /// The duplicate or the redirect failed; the server must not start then.
    #[cfg(unix)]
    pub fn take_stdout() -> io::Result<File> {
        use rustix::stdio::{dup2_stdout, stderr, stdout};
        let real = rustix::io::dup(stdout())?;
        dup2_stdout(stderr())?;
        Ok(File::from(real))
    }

    /// Moves the process's standard output to standard error and returns the real one.
    ///
    /// # Errors
    /// The duplicate or the redirect failed; the server must not start then.
    #[cfg(windows)]
    #[allow(unsafe_code)] // Win32 standard handles; every call is confined to this function.
    pub fn take_stdout() -> io::Result<File> {
        use std::os::windows::io::FromRawHandle;

        use windows_sys::Win32::Foundation::{
            DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        const STD_OUTPUT_HANDLE: u32 = -11_i32 as u32;
        const STD_ERROR_HANDLE: u32 = -12_i32 as u32;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetStdHandle(which: u32) -> HANDLE;
            fn SetStdHandle(which: u32, handle: HANDLE) -> i32;
        }
        // SAFETY: plain Win32 calls on this process's own handles; the duplicate is owned by the
        // returned `File`, and the standard handles are only re-pointed at already open handles.
        unsafe {
            let out = GetStdHandle(STD_OUTPUT_HANDLE);
            let err = GetStdHandle(STD_ERROR_HANDLE);
            if out.is_null()
                || out == INVALID_HANDLE_VALUE
                || err.is_null()
                || err == INVALID_HANDLE_VALUE
            {
                return Err(io::Error::other(
                    "asc mcp needs standard output and standard error",
                ));
            }
            let mut dup: HANDLE = std::ptr::null_mut();
            let me = GetCurrentProcess();
            if DuplicateHandle(me, out, me, &mut dup, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
                return Err(io::Error::last_os_error());
            }
            if SetStdHandle(STD_OUTPUT_HANDLE, err) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(File::from_raw_handle(dup.cast()))
        }
    }
}

/// `asc.mcp.*` entries from the config, each tagged with the layer it came from. Only the system
/// and user (global) files are trusted layers; the repository file, the environment and the
/// command line are not, so none of them can set `asc.mcp.write` or `asc.mcp.confirm`.
fn config_entries(ctx: &Ctx) -> Vec<Entry> {
    use gix_config::Source;
    let file = ctx.config.file();
    let mut out = Vec::new();
    for section in file.sections_by_name("asc").into_iter().flatten() {
        if section
            .header()
            .subsection_name()
            .map(|s| s.to_string())
            .as_deref()
            != Some("mcp")
        {
            continue;
        }
        let layer = match section.meta().source {
            Source::GitInstallation | Source::System => Layer::System,
            Source::User => Layer::User,
            Source::Worktree => Layer::Worktree,
            _ => Layer::Repo,
        };
        for key in section.body().value_names() {
            for v in section.body().values(key.as_ref()) {
                out.push(Entry::new(layer, key.as_ref(), &v.to_string()));
            }
        }
    }
    out
}

/// Serves MCP on standard input and output until the client closes it.
///
/// # Errors
/// Outside a workspace, or when the channel guard cannot be installed.
pub async fn serve(ctx: &Ctx, flags: Flags) -> Result<ExitCode> {
    let real_out = guard::take_stdout()
        .map_err(|e| CliError::Fatal(format!("asc mcp: cannot protect standard output: {e}")))?;
    stray_output_for_tests();
    let ws = ctx.ws()?;
    let backend = backend::LocalBackend::open(ctx, ws).await?;

    let mut config = McpConfig::resolve(&config_entries(ctx), &flags);
    // TODO(M10): the write tools. Until the child-verb writer exists, none is listed.
    config.write = asc_mcp::catalogue::WriteLevel::None;
    let mut cfg = HandlerConfig::new(Surface::Stdio);
    cfg.config = config;
    cfg.features = BTreeSet::new();
    let keys = KeyRing::random().map_err(|e| CliError::Fatal(format!("asc mcp: {e}")))?;
    let handler = Handler::new(backend, cfg, keys);
    handler
        .serve(
            BufReader::new(tokio::io::stdin()),
            tokio::fs::File::from_std(real_out),
            Caller::default(),
        )
        .await?;
    Ok(ExitCode::SUCCESS)
}

/// A debug build prints to standard output, in-process and from an inherited child, when
/// `ASC_MCP_TEST_STRAY_STDOUT=1`. The stdio tests use it to prove the channel guard.
fn stray_output_for_tests() {
    if !cfg!(debug_assertions)
        || std::env::var_os("ASC_MCP_TEST_STRAY_STDOUT").is_none_or(|v| v != "1")
    {
        return;
    }
    println!("stray line from in-process code");
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .status();
    }
}
