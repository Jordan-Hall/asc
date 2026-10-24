//! `asc ci checkout --repo R --rev <head> --base <base> --dir . [--mode plain] [--sparse d]`
//! (SPEC §12.3). The validated plan, cache mode, exports and the tier stack come from
//! [`asc_ci::checkout`]. This file drives the steps through the `asc` binary itself, with
//! `ASC_HOME`, `ASC_TOKEN` and the host cache set for the children: clone (which unpacks a seed
//! when the host cache is cold and reads the host cache first), the head checkout, the base ref,
//! the repository config of step 6, the `git` shim and the `$GITHUB_ENV` and `$GITHUB_PATH`
//! exports. The token is read from the environment only, never from argv.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use asc_ci::checkout::{Options, Plan};
use clap::Parser;

use crate::cli::common::{CliError, Ctx, Result};

/// Every flag of `ci checkout`, for the native table.
pub const FLAGS: &[&str] = &[
    "--repo",
    "--rev",
    "--base",
    "--dir",
    "--mode",
    "--sparse",
    "--server",
    "--host-cache",
    "--event",
    "--seed",
    "--runner-temp",
    "--remote-url",
    "--github-env",
    "--github-path",
    "--shim-dir",
    "-q",
    "--quiet",
];

#[derive(Parser, Debug)]
#[command(
    name = "ci checkout",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    /// `owner/repo`.
    #[arg(long)]
    repo: String,
    #[arg(long, env = "GITHUB_SHA", default_value = "HEAD")]
    rev: String,
    #[arg(long)]
    base: Option<String>,
    #[arg(long, default_value = ".")]
    dir: PathBuf,
    #[arg(long, default_value = "plain")]
    mode: String,
    #[arg(long)]
    sparse: Vec<String>,
    /// The asc server's public URL (`TURBO_API`).
    #[arg(long, env = "ASC_SERVER_URL")]
    server: String,
    /// The runner host's read-only cache.
    #[arg(long, env = "ASC_HOST_CACHE")]
    host_cache: Option<PathBuf>,
    #[arg(long, env = "GITHUB_EVENT_NAME", default_value = "")]
    event: String,
    /// A seed to unpack when the host cache is cold (default: the discovery document's).
    #[arg(long)]
    seed: Option<String>,
    #[arg(long, env = "RUNNER_TEMP")]
    runner_temp: PathBuf,
    /// Clone from this URL instead of `<server>/<repo>` (bucket remotes, tests).
    #[arg(long, hide = true)]
    remote_url: Option<String>,
    #[arg(long, env = "GITHUB_ENV")]
    github_env: Option<PathBuf>,
    #[arg(long, env = "GITHUB_PATH")]
    github_path: Option<PathBuf>,
    /// Where the `git` shim is installed (default `<runner temp>/asc-shim`).
    #[arg(long)]
    shim_dir: Option<PathBuf>,
    #[arg(short = 'q', long)]
    quiet: bool,
}

/// One `asc` child with the job's `ASC_HOME`, token and tier-stack config.
struct Runner<'a> {
    exe: PathBuf,
    plan: &'a Plan,
    token: &'a str,
}

impl Runner<'_> {
    fn command(&self, cwd: &Path) -> Command {
        let mut c = Command::new(&self.exe);
        c.current_dir(cwd);
        for v in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            c.env_remove(v);
        }
        c.env("ASC_HOME", &self.plan.asc_home)
            .env("ASC_TOKEN", self.token)
            .envs(self.plan.config_env())
            .stdin(Stdio::null());
        c
    }

    /// Runs `asc <args>`; returns stdout on success.
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let out = self.try_run(cwd, args)?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        Err(CliError::Fatal(format!(
            "ci checkout: asc {} failed: {}",
            args.first().copied().unwrap_or(""),
            err.trim().replace(self.token, "***")
        )))
    }

    fn try_run(&self, cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
        Ok(self.command(cwd).args(args).output()?)
    }

    /// The commit sha `rev` names, or `None`.
    fn resolve(&self, cwd: &Path, rev: &str) -> Result<Option<String>> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.try_run(cwd, &["rev-parse", "--verify", "-q", &spec])?;
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        Ok((out.status.success() && !sha.is_empty()).then_some(sha))
    }
}

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a = Args::try_parse_from(
        std::iter::once(OsString::from("ci checkout")).chain(rest.iter().cloned()),
    )
    .map_err(|e| CliError::Usage(e.to_string()))?;
    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    let runner_temp = std::path::absolute(ctx.cwd.join(&a.runner_temp))?;
    let dir = std::path::absolute(ctx.cwd.join(&a.dir))?;
    let host_cache = a
        .host_cache
        .as_deref()
        .map(|p| std::path::absolute(ctx.cwd.join(p)))
        .transpose()?;
    let shim_dir = match &a.shim_dir {
        Some(d) => std::path::absolute(ctx.cwd.join(d))?,
        None => runner_temp.join("asc-shim"),
    };
    let plan = Plan::new(Options {
        repo: a.repo.clone(),
        rev: a.rev.clone(),
        base: a.base.clone(),
        dir: dir.clone(),
        mode: a.mode.clone(),
        sparse: a.sparse.clone(),
        event: a.event.clone(),
        runner_temp: runner_temp.clone(),
        host_cache,
        server: a.server.clone(),
        token: token.clone(),
        shim_dir: shim_dir.clone(),
    })
    .map_err(|e| CliError::Fatal(format!("ci checkout: {e}")))?;

    std::fs::create_dir_all(&plan.asc_home)?;
    std::fs::create_dir_all(&plan.job_cache)?;
    let parent = dir
        .parent()
        .map(Path::to_owned)
        .unwrap_or_else(|| ctx.cwd.clone());
    std::fs::create_dir_all(&parent)?;
    let r = Runner {
        exe: std::env::current_exe()?,
        plan: &plan,
        token: &token,
    };

    // Step 3 and 4: the tier stack is in the clone's environment; a warm host cache turns the
    // discovered seed off, a cold one unpacks it into the job cache.
    let url = a
        .remote_url
        .clone()
        .unwrap_or_else(|| format!("{}/{}", a.server.trim_end_matches('/'), a.repo));
    let seed = match (&a.seed, plan.host_cache_is_cold()) {
        (Some(s), true) => s.clone(),
        (None, true) => String::new(),
        (_, false) => "none".to_owned(),
    };
    let dir_s = dir.display().to_string();
    let mut clone: Vec<&str> = vec!["clone", "-q"];
    if !seed.is_empty() {
        clone.extend(["--seed", seed.as_str()]);
    }
    // A job runs on a machine of its own, so a bucket remote can only be read through its
    // shared refs (SPEC 3.4); a server URL has no such choice.
    if url.starts_with("file:") || url.starts_with("s3:") {
        clone.extend(["--refs", "shared"]);
    }
    clone.extend([url.as_str(), dir_s.as_str()]);
    r.run(&parent, &clone)?;

    if !a.sparse.is_empty() {
        let mut sp = vec!["sparse-checkout", "set", "--cone"];
        sp.extend(a.sparse.iter().map(String::as_str));
        r.run(&dir, &sp)?;
    }

    // Head: a local ref or sha, else fetched from the server.
    let head = match r.resolve(&dir, &a.rev)? {
        Some(h) => h,
        None => {
            r.run(&dir, &["fetch", "-q", "origin", &a.rev])?;
            r.resolve(&dir, "FETCH_HEAD")?.ok_or_else(|| {
                CliError::Fatal(format!("ci checkout: cannot resolve --rev '{}'", a.rev))
            })?
        }
    };
    if r.resolve(&dir, "HEAD")?.as_deref() != Some(head.as_str()) {
        r.run(&dir, &["checkout", "-q", "--detach", &head])?;
    }

    // Base: `refs/remotes/origin/<base>` is registered (a branch name), else a sha is used as is.
    let merge_base = match a.base.as_deref() {
        None => None,
        Some(base) => {
            let tracking = format!("refs/remotes/origin/{base}");
            let base_sha = match r.resolve(&dir, &tracking)? {
                Some(s) => Some(s),
                None => {
                    let spec = format!("+refs/heads/{base}:{tracking}");
                    let _ = r.try_run(&dir, &["fetch", "-q", "origin", &spec]);
                    match r.resolve(&dir, &tracking)? {
                        Some(s) => Some(s),
                        None => r.resolve(&dir, base)?,
                    }
                }
            };
            let base_sha = base_sha.ok_or_else(|| {
                CliError::Fatal(format!("ci checkout: cannot resolve --base '{base}'"))
            })?;
            Some(r.run(&dir, &["merge-base", "HEAD", &base_sha])?)
        }
    };

    // Step 6: persist the tier stack in the repository's common config.
    for (k, v) in plan.config_entries() {
        r.run(&dir, &["config", "--local", &k, &v])?;
    }
    // The shim, then the exports for later steps.
    let shim_s = shim_dir.display().to_string();
    r.run(&parent, &["install-shim", "--dir", &shim_s])?;
    if let (Some(env_file), Some(path_file)) = (&a.github_env, &a.github_path) {
        plan.write_exports(
            &std::path::absolute(ctx.cwd.join(env_file))?,
            &std::path::absolute(ctx.cwd.join(path_file))?,
            merge_base.as_deref(),
        )
        .map_err(|e| CliError::Fatal(format!("ci checkout: {e}")))?;
    }
    if !a.quiet {
        eprintln!(
            "asc ci checkout: {} at {}",
            a.repo,
            &head[..head.len().min(12)]
        );
    }
    Ok(ExitCode::SUCCESS)
}
