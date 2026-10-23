//! `asc ci warm --repo R [--ref master] --host-cache <dir>` (SPEC §12.3 "Host cache warmer"): a
//! systemd timer runs it every minute as the host user that owns the cache. After 0-30 s of
//! jitter it clones trunk (once, with `--seed-from` when the cache is cold) or fetches it, with
//! the host cache as the writable `asc.cache.dir`, so changesets, trees and the checked-out
//! files land in the cache. PR jobs mount the same directory read-only. The plan, the seed
//! policy and the metrics are `asc_ci::warm`; `--metrics-file` is a Prometheus textfile with
//! `asc_warm_lag_seconds`, `asc_warm_objects_total` and `asc_warm_failures_total`. The token is
//! read from the environment only, never from argv.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use asc_ci::warm::{self, Outcome, Plan};
use clap::Parser;

use crate::cli::common::{CliError, Ctx, Result};

/// Every flag of `ci warm`, for the native table.
pub const FLAGS: &[&str] = &[
    "--ref",
    "--host-cache",
    "--state-dir",
    "--seed-from",
    "--jitter",
    "--metrics-file",
];

#[derive(Parser, Debug)]
#[command(
    name = "ci warm",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    /// `owner/repo`.
    #[arg(long)]
    repo: String,
    #[arg(long = "ref", default_value = "master")]
    reference: String,
    #[arg(long, env = "ASC_SERVER_URL")]
    server: String,
    /// The host cache directory (`/var/cache/asc/cache`).
    #[arg(long, default_value = "/var/cache/asc/cache")]
    host_cache: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// A seed to unpack on the first run: https, `file:` or a local path.
    #[arg(long)]
    seed_from: Option<String>,
    /// Longest random delay before the run, in seconds (0 to 30).
    #[arg(long, default_value_t = warm::MAX_JITTER_SECS)]
    jitter: u32,
    /// A Prometheus textfile to write the `asc_warm_*` metrics to.
    #[arg(long)]
    metrics_file: Option<PathBuf>,
}

struct Runner<'a> {
    exe: PathBuf,
    plan: &'a Plan,
    token: String,
}

impl Runner<'_> {
    fn try_run(&self, cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
        let mut c = Command::new(&self.exe);
        c.current_dir(cwd).args(args).stdin(Stdio::null());
        for v in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            c.env_remove(v);
        }
        c.env("ASC_HOME", &self.plan.asc_home)
            .envs(self.plan.config_env());
        if !self.token.is_empty() {
            c.env("ASC_TOKEN", &self.token);
        }
        Ok(c.output()?)
    }

    /// Runs `asc <args>`; returns stdout on success.
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let out = self.try_run(cwd, args)?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        let mut msg = err.trim().to_owned();
        if !self.token.is_empty() {
            msg = msg.replace(&self.token, "***");
        }
        Err(CliError::Fatal(format!(
            "ci warm: asc {} failed: {msg}",
            args.first().copied().unwrap_or("")
        )))
    }

    fn resolve(&self, cwd: &Path, rev: &str) -> Option<String> {
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .try_run(cwd, &["rev-parse", "--verify", "-q", &spec])
            .ok()?;
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !sha.is_empty()).then_some(sha)
    }
}

/// Clones or fetches trunk; the changesets fetched and the tip's age in seconds.
fn warm_once(plan: &Plan, r: &Runner<'_>) -> Result<Outcome> {
    std::fs::create_dir_all(&plan.asc_home)?;
    std::fs::create_dir_all(&plan.host_cache)?;
    let state = plan
        .work
        .parent()
        .map_or_else(|| plan.asc_home.clone(), Path::to_owned);
    std::fs::create_dir_all(&state)?;
    let tracking = format!("refs/remotes/origin/{}", plan.reference);
    let cloned = plan.work.join(".git").exists();
    if !cloned {
        let work = plan.work.display().to_string();
        let mut clone: Vec<String> = vec!["clone".into(), "-q".into()];
        if let Some(s) = &plan.seed {
            clone.extend(["--seed".into(), s.arg()]);
        }
        clone.extend([plan.url.clone(), work]);
        let clone: Vec<&str> = clone.iter().map(String::as_str).collect();
        r.run(&state, &clone)?;
    }
    let before = cloned.then(|| r.resolve(&plan.work, &tracking)).flatten();
    let spec = format!("+refs/heads/{}:{tracking}", plan.reference);
    r.run(&plan.work, &["fetch", "-q", "origin", &spec])?;
    let after = r
        .resolve(&plan.work, &tracking)
        .ok_or_else(|| CliError::Fatal(format!("ci warm: {} was not fetched", plan.reference)))?;
    // Materialising the tip reads its files through the cache, so they are stored there.
    r.run(&plan.work, &["checkout", "-q", "--detach", &after])?;
    let range = match &before {
        Some(b) if *b == after => None,
        Some(b) => Some(format!("{b}..{after}")),
        None => Some(after.clone()),
    };
    let objects = match range {
        None => 0,
        Some(range) => r
            .run(&plan.work, &["rev-list", "--count", &range])
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let lag_secs = r
        .run(&plan.work, &["log", "-1", "--format=%ct", &after])
        .ok()
        .and_then(|t| t.parse::<u64>().ok())
        .map(|t| now.saturating_sub(t));
    Ok(Outcome {
        ok: true,
        objects,
        lag_secs,
    })
}

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a: Args = super::changed::parse("ci warm", rest)?;
    let host_cache = std::path::absolute(ctx.cwd.join(&a.host_cache))?;
    let abs = |p: &Option<PathBuf>| -> Result<Option<PathBuf>> {
        Ok(p.as_ref()
            .map(|p| std::path::absolute(ctx.cwd.join(p)))
            .transpose()?)
    };
    let plan = Plan::new(warm::Options {
        repo: a.repo.clone(),
        reference: a.reference.clone(),
        server: a.server.clone(),
        host_cache,
        state_dir: abs(&a.state_dir)?,
        seed_from: a.seed_from.clone(),
        jitter_secs: a.jitter,
        metrics_file: abs(&a.metrics_file)?,
    })
    .map_err(|e| CliError::Fatal(format!("ci warm: {e}")))?;

    let delay = warm::jitter(plan.jitter, uuid::Uuid::new_v4().as_u128());
    tokio::time::sleep(delay).await;

    let token = std::env::var("ASC_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .unwrap_or_default();
    let runner = Runner {
        exe: std::env::current_exe()?,
        plan: &plan,
        token,
    };
    let result = warm_once(&plan, &runner);
    if let Some(file) = &plan.metrics_file {
        let outcome = result.as_ref().map_or(
            Outcome {
                ok: false,
                ..Outcome::default()
            },
            Clone::clone,
        );
        if let Err(e) = warm::write_metrics(file, &plan.repo, &outcome) {
            eprintln!("warning: ci warm: cannot write {}: {e}", file.display());
        }
    }
    result?;
    Ok(ExitCode::SUCCESS)
}
