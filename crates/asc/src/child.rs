//! Child processes (SPEC §9.4.1): aliases, editors, pagers, drivers, hooks and `git-<name>` /
//! `asc-<name>` commands spawn through [`command`], which applies the child `PATH` (the private
//! shim dir first), asc's own `-c` values as `GIT_CONFIG_PARAMETERS`, and `ASC_SHIM_DEPTH`.
//! Only `RealGit` children bypass it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use asc_git::realgit::RealGit;

use crate::env::GitEnv;
use crate::msg::Msg;

/// What every child gets.
#[derive(Debug)]
struct Setup {
    path: OsString,
    env: Vec<(OsString, OsString)>,
}

static SETUP: Mutex<Option<Arc<Setup>>> = Mutex::new(None);

/// `git[.exe]`, the shim's file name.
const GIT_FILE: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// The private shim dir for the binary at `exe`: `<exe dir>/shim`, or the directory itself when
/// the binary already lives in a directory named `shim` (it is the shim).
pub fn shim_dir_of(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    if dir.file_name().is_some_and(|n| n == "shim") {
        dir.to_owned()
    } else {
        dir.join("shim")
    }
}

/// Links `git` to `exe` inside `dir`: a hardlink on Windows, a symlink elsewhere. An existing
/// link is left alone.
fn link_git(exe: &Path, dir: &Path) -> std::io::Result<()> {
    let target = dir.join(GIT_FILE);
    if target.exists() || exe.parent() == Some(dir) {
        return Ok(());
    }
    #[cfg(windows)]
    let made = std::fs::hard_link(exe, &target);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(exe, &target);
    match made {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// Creates `<exe dir>/shim` atomically if it is missing (a temp dir renamed into place, so a
/// racing process never sees it half built) and makes sure it holds `git`.
pub fn ensure_shim_dir(exe: &Path) -> Result<PathBuf, Msg> {
    let dir = shim_dir_of(exe);
    let fail = || Msg::AscShimDir {
        dir: dir.display().to_string(),
    };
    if !dir.is_dir() {
        let parent = dir.parent().ok_or_else(fail)?;
        let tmp = parent.join(format!(".shim-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&tmp).map_err(|_| fail())?;
        let built = link_git(exe, &tmp).and_then(|()| std::fs::rename(&tmp, &dir));
        if built.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
            if !dir.is_dir() {
                return Err(fail());
            }
        }
    }
    link_git(exe, &dir).map_err(|_| fail())?;
    Ok(dir)
}

/// Prepares the child setup of this process. Call once `GitEnv` is parsed; the shim dir is
/// created on the first [`command`] call, not here.
pub fn init(env: &GitEnv, real: Option<&RealGit>) {
    let exe = std::env::current_exe().unwrap_or_default();
    let shim = shim_dir_of(&exe);
    let path = match real {
        Some(r) => r.child_path(&shim).ok(),
        None => None,
    }
    .unwrap_or_else(|| {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(shim.clone()).chain(std::env::split_paths(&inherited));
        std::env::join_paths(dirs).unwrap_or(inherited)
    });
    let setup = Arc::new(Setup {
        path,
        env: env.child_env(),
    });
    if let Ok(mut slot) = SETUP.lock() {
        *slot = Some(setup);
    }
}

fn setup() -> Arc<Setup> {
    if let Ok(slot) = SETUP.lock()
        && let Some(s) = slot.as_ref()
    {
        return s.clone();
    }
    init(&GitEnv::from_process(), None);
    SETUP
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .unwrap_or_else(|| {
            Arc::new(Setup {
                path: std::env::var_os("PATH").unwrap_or_default(),
                env: Vec::new(),
            })
        })
}

/// A command for `program` with the child `PATH`, asc's `-c` values and the shim depth applied.
/// Creates the private shim dir on first use.
pub fn command(program: impl AsRef<OsStr>) -> Result<Command, Msg> {
    let exe = std::env::current_exe().unwrap_or_default();
    ensure_shim_dir(&exe)?;
    let setup = setup();
    let mut cmd = Command::new(program);
    cmd.env("PATH", &setup.path);
    for (k, v) in &setup.env {
        cmd.env(k, v);
    }
    Ok(cmd)
}

/// The shell for `sh -c` children: Git for Windows' `usr\bin\sh.exe` on Windows, `/bin/sh`
/// elsewhere.
pub fn shell(real: Option<&RealGit>) -> PathBuf {
    real.and_then(RealGit::sh)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "sh.exe" } else { "/bin/sh" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_dir_is_beside_the_binary_or_the_binary_itself() {
        let beside = shim_dir_of(Path::new("/opt/asc/bin/asc"));
        assert_eq!(beside, PathBuf::from("/opt/asc/bin/shim"));
        let inside = shim_dir_of(Path::new("/opt/asc/bin/shim/git"));
        assert_eq!(inside, PathBuf::from("/opt/asc/bin/shim"));
    }

    #[test]
    fn shim_dir_is_created_once_with_a_git_link() {
        let t = tempfile::tempdir().unwrap();
        let exe = t.path().join(if cfg!(windows) { "asc.exe" } else { "asc" });
        std::fs::write(&exe, b"binary").unwrap();
        let dir = ensure_shim_dir(&exe).unwrap();
        assert_eq!(dir, t.path().join("shim"));
        assert!(dir.join(GIT_FILE).is_file());
        assert_eq!(std::fs::read(dir.join(GIT_FILE)).unwrap(), b"binary");
        // A second call changes nothing and leaves no temp directory behind.
        assert_eq!(ensure_shim_dir(&exe).unwrap(), dir);
        let leftovers = std::fs::read_dir(t.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".shim-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn child_path_puts_the_shim_dir_first() {
        init(&GitEnv::from_vars(&|_| None), None);
        let cmd = command("sh").unwrap();
        let path = cmd
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v)
            .unwrap()
            .to_owned();
        let first = std::env::split_paths(&path).next().unwrap();
        assert_eq!(first.file_name().unwrap(), "shim");
    }
}
