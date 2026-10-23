//! `asc daemon start|stop|status|run` (SPEC §9.1).
//!
//! `start` makes sure a daemon of this version answers (starting, restarting or replacing one
//! as needed); `stop` shuts it down; `status` reports it (exit 1 when none runs); `run` is the
//! daemon itself, in the foreground, which `start` and the CLI's autostart spawn detached.

use std::process::ExitCode;

use asc_rpc::ipc::IpcEndpoint;
use asc_state::AscHome;

use super::common::{CliError, Ctx, Result};
use crate::daemon::lifecycle::{self, Config, Probe, Stopped, SystemProcess};
use crate::daemon::{self, RunConfig, metrics, scheduler};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "daemon", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(clap::Subcommand, Debug)]
pub enum Action {
    /// Start the daemon if it is not running.
    Start,
    /// Stop the daemon.
    Stop,
    /// Say whether the daemon runs.
    Status,
    /// Run the daemon in the foreground.
    Run {
        /// Serve Prometheus metrics on this loopback address (`127.0.0.1:<port>`); off by
        /// default. `ASC_METRICS_LISTEN` is the same for a daemon that the CLI starts.
        #[arg(long = "metrics-listen", value_name = "ADDR")]
        metrics_listen: Option<String>,
    },
}

/// `daemon` serves its four actions and `run --metrics-listen`.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "daemon",
    flags: &["--metrics-listen"],
    placeholders: &[],
}];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// The lifecycle configuration and the real processes for the user's home.
pub fn system() -> Result<(Config, SystemProcess)> {
    let home = AscHome::locate()?;
    let cfg = Config::for_home(&home).map_err(fatal)?;
    let exe = std::env::current_exe().map_err(fatal)?;
    Ok((
        cfg,
        SystemProcess {
            exe,
            home: home.root().to_owned(),
        },
    ))
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) && !matches!(args.action, Action::Status) {
        return Err(fatal("the daemon is disabled by ASC_NO_DAEMON"));
    }
    let (cfg, proc) = system()?;
    match args.action {
        Action::Start => {
            let before = lifecycle::probe(&cfg).await;
            let info = lifecycle::ensure_running(&cfg, &proc).await.map_err(fatal)?;
            let again = matches!(&before, Probe::Up(i) if i.version == cfg.version);
            println!("daemon {} (pid {})", if again { "already running" } else { "started" }, info.pid);
        }
        Action::Stop => match lifecycle::stop(&cfg, &proc).await.map_err(fatal)? {
            Stopped::Stopped => println!("daemon stopped"),
            Stopped::NotRunning => println!("daemon not running"),
        },
        Action::Status => {
            return Ok(match lifecycle::probe(&cfg).await {
                Probe::Up(i) => {
                    println!("daemon running (pid {}, version {}, up {} s)", i.pid, i.version, i.uptime_ms / 1000);
                    ExitCode::SUCCESS
                }
                Probe::Hung => {
                    println!("daemon not answering");
                    ExitCode::FAILURE
                }
                Probe::Down => {
                    println!("daemon not running");
                    ExitCode::FAILURE
                }
            });
        }
        Action::Run { metrics_listen } => {
            let run_dir = cfg.run_dir.clone();
            let listen = metrics_listen
                .or_else(|| std::env::var("ASC_METRICS_LISTEN").ok().filter(|v| !v.is_empty()))
                .map(|text| metrics::parse_listen(&text))
                .transpose()
                .map_err(fatal)?;
            let home = AscHome::locate()?;
            let mut settings = scheduler::Settings::new(std::env::current_exe().map_err(fatal)?);
            let size = ctx.config.get("asc.cache.maxsize").or_else(|| ctx.config.get("asc.cache.maxSize"));
            if let Some(text) = size {
                settings.cache_max_bytes = scheduler::parse_size(&text)
                    .ok_or_else(|| fatal(format!("bad numeric config value '{text}' for 'asc.cache.maxsize'")))?;
            }
            let _watchdog = daemon::Watchdog::abort_on_hang().map_err(fatal)?;
            daemon::run_hosting(
                RunConfig {
                    endpoint: cfg.endpoint,
                    run_dir,
                    version: cfg.version,
                },
                daemon::Hosting {
                    home,
                    metrics_listen: listen,
                    backend: None,
                    settings,
                    hooks: scheduler::Hooks::default(),
                    extra_jobs: Vec::new(),
                },
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
            )
            .await
            .map_err(fatal)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Stops a daemon of another version before `state.db` is opened (SPEC §9.1). Best effort: a
/// command never fails because of the daemon. It creates nothing, not even `$ASC_HOME`.
pub async fn stop_skewed_daemon() {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return;
    }
    let Ok(endpoint) = IpcEndpoint::for_current_user() else { return };
    let proc = SystemProcess {
        exe: std::path::PathBuf::new(),
        home: std::path::PathBuf::new(),
    };
    let _ = lifecycle::stop_skewed(&Config::endpoint_only(endpoint), &proc).await;
}
