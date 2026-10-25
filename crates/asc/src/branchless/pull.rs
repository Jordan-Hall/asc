//! Cloud pull on `fetch` (SPEC §21.3.4, §20.1 D4, B15): after its own work `git fetch` asks the
//! daemon, without waiting, to run a cloud sync. The ask is best-effort: with no daemon
//! (`ASC_NO_DAEMON`, CI, tests), with the cloud off, or when anything fails, nothing happens and
//! `fetch`'s exit code, stdout and stderr stay exactly git's. The failure shows only in
//! `asc cloud status` (the daemon's side). The functions never print and never return an error.
//!
//! The ask is a marker file in the user's private `run` directory. A marker is never created
//! when the directory is absent (no daemon ever ran), and a link in its place is refused.
//!
//! The daemon's `cloud_upload` task (`daemon::cloud`) finds the marker on its next tick, removes
//! it and forces one sync. `cli/fetch.rs` calls [`after_fetch`] once its own work has finished.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use super::mode::{CloudSetting, Mode, Settings};
use crate::cli::common::Ctx;
use crate::daemon::lifecycle::{self, Config, Probe};

/// The marker the ask leaves in `run`.
pub const MARKER: &str = "cloud-sync-request";

/// The whole ask may take no longer than this; `fetch` never waits longer.
const BUDGET: Duration = Duration::from_millis(1500);

type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The daemon as the ask sees it.
pub trait Daemon: Send + Sync {
    /// Whether a daemon answers. Never starts one.
    fn running(&self) -> Fut<'_, bool>;
    /// Leaves the request.
    fn ask(&self) -> Fut<'_, io::Result<()>>;
}

/// Whether a fetch asks at all: a branchless client and repo, `asc.cloud` not `off`, a daemon
/// allowed.
pub fn wanted(s: &Settings, repo_branchless: Option<bool>, no_daemon: bool) -> bool {
    !no_daemon && s.cloud != CloudSetting::Off && s.mode(repo_branchless) == Mode::Branchless
}

/// Asks the daemon when `wanted`. Returns whether a request was left. Every failure and a
/// timeout is `false`; nothing is printed.
pub async fn request(wanted: bool, d: &dyn Daemon) -> bool {
    if !wanted {
        return false;
    }
    let go = async { d.running().await && d.ask().await.is_ok() };
    tokio::time::timeout(BUDGET, go).await.unwrap_or(false)
}

/// The production daemon: `Ping` for liveness, a marker in `run` for the ask.
struct Local {
    cfg: Config,
    run: PathBuf,
}

impl Daemon for Local {
    fn running(&self) -> Fut<'_, bool> {
        Box::pin(async move { matches!(lifecycle::probe(&self.cfg).await, Probe::Up(_)) })
    }

    fn ask(&self) -> Fut<'_, io::Result<()>> {
        let run = self.run.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || leave_marker(&run))
                .await
                .map_err(io::Error::other)?
        })
    }
}

/// Writes the marker atomically. The directory must exist and be a real directory; anything but
/// a plain file at the marker's path is refused.
fn leave_marker(run: &Path) -> io::Result<()> {
    if !std::fs::symlink_metadata(run)?.is_dir() {
        return Err(io::Error::other("run is not a directory"));
    }
    let path = run.join(MARKER);
    if let Ok(m) = std::fs::symlink_metadata(&path)
        && !m.is_file()
    {
        return Err(io::Error::other("marker is not a plain file"));
    }
    let tmp = run.join(format!("{MARKER}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, b"")?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Called by `fetch` after its own work. Never fails, never prints, bounded in time.
pub async fn after_fetch(ctx: &Ctx) {
    let getenv = |k: &str| std::env::var(k).ok();
    let Ok(settings) = Settings::resolve(&ctx.config, &ctx.env, &getenv) else {
        return;
    };
    let no_daemon = lifecycle::disabled(&|k| std::env::var_os(k));
    if !wanted(&settings, None, no_daemon) {
        return;
    }
    let Ok(ws) = ctx.ws() else { return };
    let Ok(cfg) = Config::for_home(&ws.home) else {
        return;
    };
    let d = Local {
        cfg,
        run: ws.home.run(),
    };
    let _ = request(true, &d).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        up: bool,
        fail: bool,
        hang: bool,
        asks: AtomicUsize,
    }

    impl Fake {
        fn new(up: bool, fail: bool, hang: bool) -> Self {
            Self {
                up,
                fail,
                hang,
                asks: AtomicUsize::new(0),
            }
        }
    }

    impl Daemon for Fake {
        fn running(&self) -> Fut<'_, bool> {
            Box::pin(async move {
                if self.hang {
                    std::future::pending::<()>().await;
                }
                self.up
            })
        }
        fn ask(&self) -> Fut<'_, io::Result<()>> {
            Box::pin(async move {
                self.asks.fetch_add(1, Ordering::SeqCst);
                if self.fail {
                    Err(io::Error::other("boom"))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn settings(branchless: bool, cloud: CloudSetting) -> Settings {
        Settings {
            branchless,
            cloud,
            ..Settings::default()
        }
    }

    #[test]
    fn wanted_follows_mode_cloud_and_daemon() {
        let on = settings(true, CloudSetting::Auto);
        assert!(wanted(&on, None, false));
        assert!(!wanted(&on, None, true), "no daemon");
        assert!(!wanted(&on, Some(false), false), "repo is not branchless");
        assert!(!wanted(&settings(false, CloudSetting::Auto), None, false));
        assert!(!wanted(&settings(true, CloudSetting::Off), None, false));
    }

    #[tokio::test]
    async fn asks_only_a_running_daemon() {
        let d = Fake::new(true, false, false);
        assert!(request(true, &d).await);
        assert_eq!(d.asks.load(Ordering::SeqCst), 1);
        let down = Fake::new(false, false, false);
        assert!(!request(true, &down).await);
        assert_eq!(down.asks.load(Ordering::SeqCst), 0);
        assert!(!request(false, &d).await);
        assert_eq!(d.asks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_or_hung_ask_is_only_false() {
        assert!(!request(true, &Fake::new(true, true, false)).await);
        let t = std::time::Instant::now();
        assert!(!request(true, &Fake::new(true, false, true)).await);
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn marker_needs_a_real_run_directory() {
        let t = tempfile::tempdir().unwrap();
        assert!(leave_marker(&t.path().join("absent")).is_err());
        assert!(!t.path().join("absent").exists(), "never creates run");
        leave_marker(t.path()).unwrap();
        assert!(t.path().join(MARKER).is_file());
        leave_marker(t.path()).unwrap();
        let u = tempfile::tempdir().unwrap();
        std::fs::create_dir(u.path().join(MARKER)).unwrap();
        assert!(leave_marker(u.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_run_directory_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = t.path().join("run");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(leave_marker(&link).is_err());
        assert!(!real.join(MARKER).exists());
    }
}
