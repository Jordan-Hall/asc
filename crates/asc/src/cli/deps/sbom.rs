//! `asc deps sbom`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, ecosystems, remote_with_token};
use crate::cli::agent::conn::Conn;
use crate::cli::codeintel::{clean_str, with_stdout};
use crate::cli::common::{CliError, Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct SbomArgs {
    /// The revision to read; trunk when absent.
    #[arg(long)]
    rev: Option<String>,
    /// Only this ecosystem (OSV name or alias); repeat.
    #[arg(long = "ecosystem")]
    ecosystems: Vec<String>,
    /// Write the CycloneDX document to this file instead of standard output.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Replace an existing --out file.
    #[arg(long)]
    force: bool,
    #[arg(long)]
    remote: Option<String>,
    /// Take the credential from this environment variable (needs --remote <https URL>).
    #[arg(long = "token-env")]
    token_env: Option<String>,
}

fn io_err(path: &Path, what: &str, e: &std::io::Error) -> CliError {
    CliError::Fatal(format!(
        "asc: cannot {what} {}: {e}",
        clean_str(&path.display().to_string())
    ))
}

/// What an atomic rename that must not replace did.
enum Moved {
    Done,
    /// `out` exists.
    Exists,
    /// The platform or the file system has no such rename; the caller falls back.
    Unsupported,
}

/// Renames `from` to `to` and answers `Exists` instead of replacing `to`.
#[cfg(windows)]
#[allow(unsafe_code)] // one Win32 call on two NUL-terminated wide strings that outlive it
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<Moved> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
    let (f, t) = (wide(from), wide(to));
    // Flags 0: without MOVEFILE_REPLACE_EXISTING the call fails when `to` exists.
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } != 0 {
        return Ok(Moved::Done);
    }
    let e = std::io::Error::last_os_error();
    // ERROR_FILE_EXISTS 80, ERROR_ALREADY_EXISTS 183.
    match e.raw_os_error() {
        Some(80 | 183) => Ok(Moved::Exists),
        _ => Err(e),
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<Moved> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(Moved::Done),
        Err(Errno::EXIST) => Ok(Moved::Exists),
        // An old kernel or a file system without the flag: the caller falls back.
        Err(Errno::NOSYS | Errno::INVAL | Errno::NOTSUP) => Ok(Moved::Unsupported),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(any(windows, target_os = "linux", target_vendor = "apple")))]
fn rename_noreplace(_: &Path, _: &Path) -> std::io::Result<Moved> {
    Ok(Moved::Unsupported)
}

/// Moves `from` to `to` without ever replacing `to`: the atomic rename where there is one, else a hard link
/// (which also fails when `to` exists) and removal of `from`.
fn move_noreplace(
    from: &Path,
    to: &Path,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<Moved>,
) -> std::io::Result<Moved> {
    match rename(from, to)? {
        Moved::Unsupported => match std::fs::hard_link(from, to) {
            Ok(()) => {
                let _ = std::fs::remove_file(from);
                Ok(Moved::Done)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Moved::Exists),
            Err(e) => Err(e),
        },
        other => Ok(other),
    }
}

/// Makes the rename itself durable. Windows has no directory sync; a file system may refuse one, which is
/// no failure once the rename has happened.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        let _ = std::fs::File::open(dir).and_then(|f| f.sync_all());
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

fn exists_error(out: &Path) -> CliError {
    bad(format!(
        "{} exists; --force replaces it",
        clean_str(&out.display().to_string())
    ))
}

/// Writes `bytes` to `out` through a temporary file in the same directory, so a reader never sees half a
/// document. An existing `out` is kept unless `force`.
pub(crate) fn write_out(out: &Path, bytes: &[u8], force: bool) -> Result<()> {
    if !force && std::fs::symlink_metadata(out).is_ok() {
        return Err(exists_error(out));
    }
    let dir = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if out.file_name().is_none() {
        return Err(bad("--out is a file path"));
    }
    let mut tmp = None;
    for n in 0..100u32 {
        let candidate = dir.join(format!(".asc-sbom.{}.{n}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => {
                tmp = Some((candidate, f));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(&candidate, "create", &e)),
        }
    }
    let (path, mut file) =
        tmp.ok_or_else(|| CliError::Fatal("asc: cannot create a temporary file".to_owned()))?;
    let done = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_err(&path, "write", &e))
        .and_then(|()| {
            drop(file);
            if force {
                std::fs::rename(&path, out).map_err(|e| io_err(out, "replace", &e))
            } else {
                match move_noreplace(&path, out, rename_noreplace) {
                    Ok(Moved::Done) => Ok(()),
                    Ok(_) => Err(exists_error(out)),
                    Err(e) => Err(io_err(out, "write", &e)),
                }
            }
        });
    if done.is_ok() {
        sync_dir(dir);
    } else {
        let _ = std::fs::remove_file(&path);
    }
    done
}

/// A document for a terminal has to be UTF-8 JSON without a character that could drive the terminal (those
/// `clean_str` escapes, but for line breaks and tabs); anything else is for a file or a pipe.
fn check_for_terminal(document: &[u8]) -> Result<()> {
    let refuse = || {
        CliError::Fatal(
            "asc: the document is not plain UTF-8 JSON that is safe to show; use --out or a pipe"
                .to_owned(),
        )
    };
    let text = std::str::from_utf8(document).map_err(|_| refuse())?;
    serde_json::from_str::<serde_json::Value>(text).map_err(|_| refuse())?;
    for c in text.chars() {
        let mut buf = [0u8; 4];
        let one: &str = c.encode_utf8(&mut buf);
        if !matches!(c, '\n' | '\r' | '\t') && clean_str(one) != one {
            return Err(refuse());
        }
    }
    Ok(())
}

pub async fn run(ctx: &Ctx, a: SbomArgs) -> Result<ExitCode> {
    use std::io::IsTerminal;
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let ecos = ecosystems(&a.ecosystems)?;
    if let Some(r) = &a.rev
        && (r.is_empty() || r.len() > 1024)
    {
        return Err(bad("--rev is a revision"));
    }
    if a.force && a.out.is_none() {
        return Err(bad("--force needs --out"));
    }
    if let Some(p) = &a.out
        && !a.force
        && std::fs::symlink_metadata(p).is_ok()
    {
        return Err(bad(format!(
            "{} exists; --force replaces it",
            clean_str(&p.display().to_string())
        )));
    }
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::SbomRequest {
                repo: Some(repo.clone()),
                rev: a.rev.clone().unwrap_or_default(),
                ecosystems: ecos.clone(),
            };
            async move { Ok(client(&s).sbom(s.wire.request(m, None)).await?.into_inner()) }
        })
        .await?;
    match &a.out {
        Some(p) => write_out(p, &r.document, a.force)?,
        None => {
            if std::io::stdout().is_terminal() {
                check_for_terminal(&r.document)?;
            }
            with_stdout(|out| {
                out.write_all(&r.document)?;
                Ok(ExitCode::SUCCESS)
            })?;
        }
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "# {} components", r.components);
    if r.truncated {
        let _ = writeln!(
            err,
            "# warning: more manifests or packages than the limits allow; the document is partial"
        );
    }
    for n in &r.notes {
        let _ = writeln!(err, "# {}", clean_str(n));
    }
    Ok(if r.truncated {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_refuses_to_overwrite_without_force_and_replaces_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("bom.json");
        write_out(&out, b"{\"a\":1}", false).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"{\"a\":1}");
        assert!(write_out(&out, b"{}", false).is_err());
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"{\"a\":1}",
            "the file is untouched"
        );
        write_out(&out, b"{\"b\":2}", true).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"{\"b\":2}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temporary file is left: {names:?}");
    }

    #[test]
    fn a_failed_write_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("missing").join("bom.json");
        assert!(write_out(&out, b"{}", false).is_err());
        assert!(!out.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_terminal_gets_only_plain_utf8_json() {
        assert!(check_for_terminal(b"{\"bomFormat\":\"CycloneDX\"}").is_ok());
        assert!(check_for_terminal(b"{\n\t\"a\": 1\r\n}").is_ok());
        assert!(check_for_terminal(b"\x1b]0;x\x07").is_err());
        assert!(check_for_terminal(b"{\"a\":\"\xff\"}").is_err());
        for c in [
            '\u{9b}', '\u{85}', '\u{7f}', '\u{202e}', '\u{2066}', '\u{2028}',
        ] {
            let doc = format!("{{\"a\":\"x{c}y\"}}");
            assert!(check_for_terminal(doc.as_bytes()).is_err(), "{c:?}");
        }
    }

    #[test]
    fn an_unsupported_rename_falls_back_and_still_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&from, b"new").unwrap();
        let moved = move_noreplace(&from, &to, |_, _| Ok(Moved::Unsupported)).unwrap();
        assert!(matches!(moved, Moved::Done));
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
        std::fs::write(&from, b"other").unwrap();
        let moved = move_noreplace(&from, &to, |_, _| Ok(Moved::Unsupported)).unwrap();
        assert!(matches!(moved, Moved::Exists));
        let moved = move_noreplace(&from, &to, rename_noreplace).unwrap();
        assert!(matches!(moved, Moved::Exists));
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
    }
}
