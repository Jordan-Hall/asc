//! `asc serve [migrate] [--config <file>] [--role <role>] [--upstream <url>]` (SPEC §8.1).
//!
//! `migrate` applies the schema and exits; the server itself never migrates at start.
//!
//! `asc serve token create|list|revoke` manages the standalone token store (SPEC §15.1, M30):
//! `create --repo <r>... [--user <u> | --deploy | --ci] [--read|--write|--admin] [--expires 30d]
//! [--label <text>]` prints the token on stdout once and its details on stderr.

#[cfg(not(feature = "serve"))]
stub!("serve");

#[cfg(feature = "serve")]
pub use real::*;

#[cfg(feature = "serve")]
mod real {
    use std::path::PathBuf;
    use std::process::ExitCode;

    use asc_rpc::auth::Access;
    use asc_serve::authz::token_cli::{self, CreateArgs};
    use asc_serve::authz::token_store::TokenKind;
    use asc_serve::{Role, ServeConfig};

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
        /// Manages the standalone token store (`[auth] provider = "token_store"`).
        Token {
            #[command(subcommand)]
            cmd: TokenCmd,
        },
    }

    #[derive(clap::Subcommand, Debug)]
    pub enum TokenCmd {
        /// Mints a token and prints it once.
        Create(CreateFlags),
        /// Lists tokens (never their secrets).
        List,
        /// Revokes a token by its id.
        Revoke { id: String },
    }

    #[derive(clap::Args, Debug)]
    pub struct CreateFlags {
        /// A repository (`owner/name`) or, for a personal or CI token, `owner/*`; repeatable.
        #[arg(long, required = true)]
        pub repo: Vec<String>,
        /// The user a personal token acts as (the default kind).
        #[arg(long, conflicts_with_all = ["deploy", "ci"])]
        pub user: Option<String>,
        /// A deploy token: one repository, no user.
        #[arg(long, conflicts_with = "ci")]
        pub deploy: bool,
        /// A CI token: no user, never admin, always expires (1d by default, 30d at most).
        #[arg(long)]
        pub ci: bool,
        /// Read access (the default).
        #[arg(long, group = "level")]
        pub read: bool,
        /// Read and write access.
        #[arg(long, group = "level")]
        pub write: bool,
        /// Read, write and admin access (personal tokens only).
        #[arg(long, group = "level")]
        pub admin: bool,
        /// Lifetime such as `30d`, `12h` or `2w`.
        #[arg(long)]
        pub expires: Option<String>,
        /// A note shown by `list`.
        #[arg(long, default_value = "")]
        pub label: String,
    }

    impl CreateFlags {
        fn into_args(self) -> CreateArgs {
            CreateArgs {
                kind: if self.deploy {
                    TokenKind::Deploy
                } else if self.ci {
                    TokenKind::Ci
                } else {
                    TokenKind::Pat
                },
                user: self.user,
                repos: self.repo,
                access: if self.admin {
                    Access::Admin
                } else if self.write {
                    Access::Write
                } else {
                    Access::Read
                },
                expires: self.expires,
                label: self.label,
            }
        }
    }

    pub const NATIVE: &[NativeSpec] = &[NativeSpec {
        cmd: "serve",
        flags: &[
            "--config",
            "--role",
            "--upstream",
            "--repo",
            "--user",
            "--deploy",
            "--ci",
            "--read",
            "--write",
            "--admin",
            "--expires",
            "--label",
        ],
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
            Some(Sub::Token { cmd }) => match cmd {
                TokenCmd::Create(flags) => {
                    let made = token_cli::create(&cfg, flags.into_args())
                        .await
                        .map_err(fatal)?;
                    eprint!("{}", made.summary);
                    println!("{}", made.token);
                }
                TokenCmd::List => print!("{}", token_cli::list(&cfg).await.map_err(fatal)?),
                TokenCmd::Revoke { id } => {
                    print!("{}", token_cli::revoke(&cfg, &id).await.map_err(fatal)?);
                }
            },
            None => {
                let authorizer = asc_serve::authz::authorizer(&cfg).await.map_err(fatal)?;
                let stop = async {
                    let _ = tokio::signal::ctrl_c().await;
                };
                asc_serve::serve(cfg, authorizer, stop)
                    .await
                    .map_err(fatal)?;
            }
        }
        Ok(ExitCode::SUCCESS)
    }
}
