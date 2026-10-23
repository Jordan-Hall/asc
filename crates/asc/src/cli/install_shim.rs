//! `asc install-shim [--dir <d>]` (SPEC §11.4): puts `git[.exe]` beside `asc` in
//! `<exe dir>/shim` (a hardlink on Windows, a symlink on Unix) and prints the PATH line.

use std::path::{Component, Path, PathBuf, Prefix};
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use crate::child;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "install-shim", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The directory to hold `git`; `<exe dir>/shim` by default.
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "install-shim",
    flags: &["--dir"],
    placeholders: &[],
}];

const GIT_FILE: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// The volume of an absolute path: the drive letter or UNC share; `None` without a prefix.
fn volume(path: &Path) -> Option<String> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    Some(match prefix.kind() {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => char::from(d).to_ascii_uppercase().to_string(),
        _ => prefix.as_os_str().to_string_lossy().to_ascii_lowercase(),
    })
}

/// Whether a hardlink between `a` and `b` can exist.
fn same_volume(a: &Path, b: &Path) -> bool {
    volume(a) == volume(b)
}

/// Points `<dir>/git[.exe]` at `exe`, replacing a stale link atomically.
fn relink(exe: &Path, dir: &Path) -> std::io::Result<()> {
    if exe.parent() == Some(dir) {
        return Ok(()); // already running as the shim
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".git-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    #[cfg(windows)]
    std::fs::hard_link(exe, &tmp)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(exe, &tmp)?;
    std::fs::rename(&tmp, dir.join(GIT_FILE)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The line that puts `dir` first on `PATH`.
fn path_line(dir: &Path) -> String {
    let shown = dir.display().to_string();
    if cfg!(windows) {
        format!("set PATH={shown};%PATH%")
    } else {
        format!("export PATH=\"{shown}:$PATH\"")
    }
}

/// The running binary by the path it was started through. Windows keeps that path in
/// `current_exe`; Linux resolves symlinks there, which would turn a stable `current` link into a
/// version directory. The started-through path is used only when it names the same file.
fn invoked_exe() -> std::io::Result<PathBuf> {
    let real = std::env::current_exe()?;
    #[cfg(unix)]
    if let Some(arg0) = std::env::args_os().next().map(PathBuf::from) {
        if arg0.components().count() > 1 {
            let abs = if arg0.is_absolute() { arg0 } else { std::env::current_dir()?.join(arg0) };
            let same = std::fs::canonicalize(&abs).ok().zip(std::fs::canonicalize(&real).ok());
            if same.is_some_and(|(a, b)| a == b) {
                return Ok(abs);
            }
        }
    }
    Ok(real)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let exe = invoked_exe()?;
    let dir = match &a.dir {
        Some(d) => ctx.cwd.join(d),
        None => child::shim_dir_of(&exe),
    };
    if !same_volume(&exe, &dir) {
        return Err(CliError::Raw(
            "fatal: asc: --dir must be on the same volume as asc.exe".to_owned(),
            128,
        ));
    }
    relink(&exe, &dir).map_err(|e| CliError::Fatal(format!("asc: cannot install {}: {e}", dir.display())))?;
    println!("{}", path_line(&dir));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn a_dir_on_another_drive_is_not_the_same_volume() {
        assert!(same_volume(Path::new(r"C:\asc\asc.exe"), Path::new(r"c:\other\shim")));
        assert!(same_volume(Path::new(r"\\?\C:\asc\asc.exe"), Path::new(r"C:\shim")));
        assert!(!same_volume(Path::new(r"C:\asc\asc.exe"), Path::new(r"D:\shim")));
    }

    #[test]
    fn relink_replaces_a_stale_link() {
        let t = tempfile::tempdir().unwrap();
        let (old, new) = (t.path().join("old"), t.path().join("new"));
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        let dir = t.path().join("shim");
        relink(&old, &dir).unwrap();
        relink(&new, &dir).unwrap();
        assert_eq!(std::fs::read(dir.join(GIT_FILE)).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp link is left");
    }
}
