//! Starting the helper (SPEC §23.2.5): only when the workspace is idle, never twice for one
//! repository, and at most once a minute per workspace. The helper is a separate process
//! (`asc codeintel worker`); the daemon never builds an index itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Least time between two starts for one workspace.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);

static LAST: Mutex<Option<HashMap<PathBuf, Instant>>> = Mutex::new(None);

/// True (and records the start) when a helper for `mount` may start now.
fn due(mount: &Path, now: Instant) -> bool {
    let Ok(mut g) = LAST.lock() else {
        return false;
    };
    let map = g.get_or_insert_with(HashMap::new);
    if map.len() > 1024 {
        map.retain(|_, t| now.saturating_duration_since(*t) < MIN_INTERVAL);
    }
    match map.get(mount) {
        Some(t) if now.saturating_duration_since(*t) < MIN_INTERVAL => false,
        _ => {
            map.insert(mount.to_path_buf(), now);
            true
        }
    }
}

/// Starts `asc codeintel worker` for the workspace mounted at `mount` when it is due, the
/// workspace is idle (`common_dir` holds its `HEAD` and index) and no helper holds the
/// repository's lock. Returns whether a helper was started. Never blocks and never fails the
/// caller: a failure to start is "no index yet".
pub fn nudge(mount: &Path, common_dir: &Path, locked: bool) -> bool {
    if locked || !super::worker::is_idle(common_dir) || !due(mount, Instant::now()) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("-C")
        .arg(mount)
        .args(["codeintel", "worker"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // IDLE_PRIORITY_CLASS | CREATE_NO_WINDOW | DETACHED_PROCESS
        cmd.creation_flags(0x0000_0040 | 0x0800_0000 | 0x0000_0008);
    }
    cmd.spawn().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_is_started_at_most_once_a_minute() {
        let m = Path::new("/ws/due-test");
        let t = Instant::now();
        assert!(due(m, t));
        assert!(!due(m, t + Duration::from_secs(30)));
        assert!(due(m, t + MIN_INTERVAL + Duration::from_secs(1)));
    }

    #[test]
    fn a_locked_or_busy_workspace_does_not_start_a_helper() {
        let d = tempfile::tempdir().unwrap();
        assert!(!nudge(Path::new("/ws/locked-test"), d.path(), true));
        std::fs::write(d.path().join("HEAD"), b"x").unwrap();
        assert!(!nudge(Path::new("/ws/busy-test"), d.path(), false));
    }
}
