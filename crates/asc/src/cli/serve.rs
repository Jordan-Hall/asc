//! `asc serve [migrate] [--config <file>] [--role <role>] [--upstream <url>]` (SPEC §8.1).
//!
//! `migrate` applies the schema and exits; the server itself never migrates at start.

#[cfg(not(feature = "serve"))]
stub!("serve");

#[cfg(feature = "serve")]
pub use real::*;

#[cfg(feature = "serve")]
mod real {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;

    use asc_serve::{DenyAll, Role, ServeConfig};

    use crate::cli::common::{CliError, Ctx, Result};
    use crate::native::NativeSpec;

    /// The config file read when `--config` is absent.
    pub const DEFAULT_CONFIG: &str = "asc-serve.toml";

    #[derive(clap::Parser, Debug)]
    #[command(name = "serve", disable_help_flag = true, disable_version_flag = true)]
    pub struct Args {
        #[command(subcommand)]
        pub cmd: Option<Sub>,
        /// The `asc-serve.toml` to load.
        #[arg(long, global = true, env = "ASC_SERVE_CONFIG")]
        pub config: Option<PathBuf>,
        /// Overrides `role` in the config.
        #[arg(long, global = true, value_parser = ["primary", "cache", "indexer"])]
        pub role: Option<String>,
        /// Overrides `upstream` in the config (cache role).
        #[arg(long, global = true)]
        pub upstream: Option<String>,
    }

    #[derive(clap::Subcommand, Debug)]
    pub enum Sub {
        /// Applies the database schema, then exits.
        Migrate,
    }

    pub const NATIVE: &[NativeSpec] = &[NativeSpec {
        cmd: "serve",
        flags: &["--config", "--role", "--upstream"],
        placeholders: &[],
    }];

    fn fatal(e: impl std::fmt::Display) -> CliError {
        CliError::Fatal(format!("asc serve: {e}"))
    }

    /// The config with the command-line overrides applied and validated.
    fn config(a: &Args, ctx: &Ctx) -> Result<ServeConfig> {
        let path = ctx.cwd.join(
            a.config
                .as_deref()
                .unwrap_or_else(|| DEFAULT_CONFIG.as_ref()),
        );
        let mut cfg = ServeConfig::load(&path).map_err(fatal)?;
        if let Some(role) = a.role.as_deref() {
            cfg.role = match role {
                "cache" => Role::Cache,
                "indexer" => Role::Indexer,
                _ => Role::Primary,
            };
        }
        if let Some(upstream) = &a.upstream {
            cfg.upstream = Some(upstream.clone());
        }
        cfg.validate().map_err(fatal)?;
        Ok(cfg)
    }

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        let cfg = config(&a, ctx)?;
        match a.cmd {
            Some(Sub::Migrate) => asc_serve::run_migrate(&cfg).await.map_err(fatal)?,
            None => {
                let stop = async {
                    let _ = tokio::signal::ctrl_c().await;
                };
                asc_serve::serve(cfg, Arc::new(DenyAll), stop)
                    .await
                    .map_err(fatal)?;
            }
        }
        Ok(ExitCode::SUCCESS)
    }
}
