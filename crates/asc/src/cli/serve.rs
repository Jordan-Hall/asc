//! `asc serve [migrate] [--config <file>] [--role <role>] [--upstream <url>]` (SPEC §8.1).
//!
//! `migrate` applies the schema and exits; the server itself never migrates at start.
//!
//! `asc serve token create|list|revoke` manages the standalone token store (SPEC §15.1, M30):
//! `create --repo <r>... [--user <u> | --deploy | --ci] [--read|--write|--admin] [--expires 30d]
//! [--label <text>]` prints the token on stdout once and its details on stderr.
//!
//! `asc serve ssh-key add|list|revoke` manages SSH public keys in the token store (SPEC §25.11.3;
//! build feature `git-ssh`): `add --user <u> --key-file <pub> [--title <t>] [--expires 30d]`,
//! `add --repo <r> --deploy [--write] --key-file <pub>`, `list`, `revoke <fingerprint|id>`.
//!
//! `asc serve ssh host-key show|rotate` handles the SSH listener's host keys (SPEC §25.11.6; build
//! feature `git-ssh`): `show` prints each key's `SHA256:` fingerprint and a `known_hosts` line,
//! `rotate` writes a new first host key.

#[cfg(not(feature = "serve"))]
stub!("serve");

#[cfg(feature = "serve")]
pub use real::*;

#[cfg(feature = "serve")]
mod real {
    use std::path::PathBuf;
    use std::process::ExitCode;

    use asc_rpc::auth::Access;
    #[cfg(feature = "git-ssh")]
    use asc_serve::authz::token_cli::SshKeyAddArgs;
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
        /// Operator commands for the ASC cutover (SPEC §14.3): import, verify, outbox, mode.
        Admin {
            #[command(subcommand)]
            cmd: AdminCmd,
        },
        /// Manages the standalone token store (`[auth] provider = "token_store"`).
        Token {
            #[command(subcommand)]
            cmd: TokenCmd,
        },
        /// Manages SSH public keys in the token store (user keys and deploy keys).
        #[cfg(feature = "git-ssh")]
        SshKey {
            #[command(subcommand)]
            cmd: SshKeyCmd,
        },
        /// The SSH listener's own settings (host keys).
        #[cfg(feature = "git-ssh")]
        Ssh {
            #[command(subcommand)]
            cmd: SshCmd,
        },
    }

    #[derive(clap::Subcommand, Debug)]
    pub enum AdminCmd {
        /// Imports git history into a mirror-mode repository (the only way history gets in).
        Import {
            #[arg(long)]
            repo: String,
            /// Creates the repository in mirror mode when it does not exist.
            #[arg(long)]
            init: bool,
            /// The git source, without credentials; the config supplies those.
            #[arg(long)]
            source: Option<String>,
        },
        /// Checks that every git commit's export equals git; exits 1 on any mismatch.
        VerifyImport {
            #[arg(long = "repo", required = true)]
            repos: Vec<String>,
        },
        /// Checks every ref's closure and the git map round trip.
        Verify {
            #[arg(long = "repo")]
            repos: Vec<String>,
            #[arg(long)]
            all: bool,
        },
        /// Reports, or with `--wait` waits for, the undelivered outbox rows.
        Outbox {
            #[command(subcommand)]
            cmd: OutboxCmd,
        },
        /// Sets `repos.mode`, `git_reads` and `bridge`.
        SetMode {
            #[arg(long)]
            repo: String,
            #[arg(long, value_parser = ["mirror", "primary"])]
            mode: Option<String>,
            #[arg(long, value_parser = ["git", "asc"])]
            git_reads: Option<String>,
            /// Deprecated alias of `--git-reads`.
            #[arg(long, value_parser = ["git", "asc"], hide = true, conflicts_with = "git_reads")]
            forgejo_reads: Option<String>,
            #[arg(long, value_parser = ["off", "lazy", "eager"])]
            bridge: Option<String>,
            /// Skips the clean-import check of a flip to primary.
            #[arg(long)]
            force: bool,
        },
    }

    #[derive(clap::Subcommand, Debug)]
    pub enum OutboxCmd {
        Drain {
            #[arg(long)]
            repo: Option<String>,
            #[arg(long)]
            wait: bool,
            /// Seconds to wait.
            #[arg(long, default_value_t = 300)]
            timeout: u64,
        },
    }

    #[cfg(feature = "git-ssh")]
    #[derive(clap::Subcommand, Debug)]
    pub enum SshCmd {
        /// Shows or rotates the host keys clients are asked to trust.
        HostKey {
            #[command(subcommand)]
            cmd: HostKeyCmd,
        },
    }

    #[cfg(feature = "git-ssh")]
    #[derive(clap::Subcommand, Debug)]
    pub enum HostKeyCmd {
        /// Prints each host key's fingerprint and a `known_hosts` line.
        Show,
        /// Writes a new host key; clients will see a changed-key warning.
        Rotate,
    }

    #[cfg(feature = "git-ssh")]
    #[derive(clap::Subcommand, Debug)]
    pub enum SshKeyCmd {
        /// Registers a public key (an `authorized_keys` line without options).
        Add(SshKeyAddFlags),
        /// Lists keys (never their blobs).
        List,
        /// Revokes a key by its `SHA256:` fingerprint or its id.
        Revoke { key: String },
    }

    #[cfg(feature = "git-ssh")]
    #[derive(clap::Args, Debug)]
    pub struct SshKeyAddFlags {
        /// The user a user key acts as, with that user's rights.
        #[arg(long, conflicts_with_all = ["repo", "deploy", "write"], required_unless_present = "deploy")]
        pub user: Option<String>,
        /// The one repository (`owner/name`) a deploy key reaches.
        #[arg(long, requires = "deploy")]
        pub repo: Option<String>,
        /// A deploy key: bound to `--repo`, read-only unless `--write`.
        #[arg(long, requires = "repo")]
        pub deploy: bool,
        /// A deploy key may push.
        #[arg(long, requires = "deploy")]
        pub write: bool,
        /// The public key file: one `authorized_keys` line (type, base64, comment).
        #[arg(long)]
        pub key_file: PathBuf,
        /// A note shown by `list` (the key's comment by default).
        #[arg(long, default_value = "")]
        pub title: String,
        /// Lifetime such as `30d`, `12h` or `2w`.
        #[arg(long)]
        pub expires: Option<String>,
    }

    #[cfg(feature = "git-ssh")]
    impl SshKeyAddFlags {
        pub fn into_args(self) -> SshKeyAddArgs {
            SshKeyAddArgs {
                user: self.user,
                repo: self.repo,
                deploy: self.deploy,
                write: self.write,
                key_file: self.key_file,
                title: self.title,
                expires: self.expires,
            }
        }
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
            "--key-file",
            "--title",
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

    async fn admin(cfg: &ServeConfig, cmd: AdminCmd) -> Result<ExitCode> {
        use asc_serve::admin_cli as ac;
        use std::time::Duration;
        let cli = ac::open(cfg).await.map_err(fatal)?;
        let report = match cmd {
            AdminCmd::Import { repo, init, source } => {
                cli.import(&repo, init, source.as_deref()).await
            }
            AdminCmd::VerifyImport { repos } => cli.verify_import(&repos).await,
            AdminCmd::Verify { repos, all } => cli.verify(&repos, all).await,
            AdminCmd::Outbox {
                cmd:
                    OutboxCmd::Drain {
                        repo,
                        wait,
                        timeout,
                    },
            } => {
                cli.outbox_drain(
                    repo.as_deref(),
                    wait,
                    Duration::from_secs(timeout),
                    ac::DEFAULT_DRAIN_POLL,
                )
                .await
            }
            AdminCmd::SetMode {
                repo,
                mode,
                git_reads,
                forgejo_reads,
                bridge,
                force,
            } => {
                cli.set_mode(&ac::SetMode {
                    repo,
                    mode,
                    git_reads: git_reads.or(forgejo_reads),
                    bridge,
                    force,
                })
                .await
            }
        }
        .map_err(fatal)?;
        print!("{}", report.text);
        Ok(if report.ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        let cfg = config(&a, ctx)?;
        match a.cmd {
            Some(Sub::Migrate) => asc_serve::run_migrate(&cfg).await.map_err(fatal)?,
            Some(Sub::Admin { cmd }) => return admin(&cfg, cmd).await,
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
            #[cfg(feature = "git-ssh")]
            Some(Sub::SshKey { cmd }) => match cmd {
                SshKeyCmd::Add(flags) => {
                    let said = token_cli::ssh_key_add(&cfg, flags.into_args())
                        .await
                        .map_err(fatal)?;
                    print!("{said}");
                }
                SshKeyCmd::List => {
                    print!("{}", token_cli::ssh_key_list(&cfg).await.map_err(fatal)?);
                }
                SshKeyCmd::Revoke { key } => {
                    print!(
                        "{}",
                        token_cli::ssh_key_revoke(&cfg, &key).await.map_err(fatal)?
                    );
                }
            },
            #[cfg(feature = "git-ssh")]
            Some(Sub::Ssh {
                cmd: SshCmd::HostKey { cmd },
            }) => match cmd {
                HostKeyCmd::Show => {
                    print!("{}", asc_serve::ssh::host_key_show(&cfg).map_err(fatal)?)
                }
                HostKeyCmd::Rotate => {
                    print!("{}", asc_serve::ssh::host_key_rotate(&cfg).map_err(fatal)?);
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

#[cfg(all(test, feature = "serve", feature = "git-ssh"))]
mod ssh_key_tests {
    use clap::Parser as _;

    use super::{Args, HostKeyCmd, SshCmd, SshKeyCmd, Sub};

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("serve").chain(args.iter().copied()))
    }

    #[test]
    fn host_key_show_and_rotate_parse() {
        for (word, want_rotate) in [("show", false), ("rotate", true)] {
            let a = parse(&["ssh", "host-key", word]).unwrap();
            let Some(Sub::Ssh {
                cmd: SshCmd::HostKey { cmd },
            }) = a.cmd
            else {
                panic!("not ssh host-key");
            };
            assert_eq!(matches!(cmd, HostKeyCmd::Rotate), want_rotate);
        }
        assert!(parse(&["ssh", "host-key"]).is_err());
        assert!(parse(&["ssh", "host-key", "delete"]).is_err());
        assert!(parse(&["ssh"]).is_err());
    }

    #[test]
    fn user_and_deploy_keys_parse() {
        let a = parse(&[
            "ssh-key",
            "add",
            "--user",
            "alice",
            "--key-file",
            "k.pub",
            "--title",
            "t",
            "--expires",
            "30d",
        ])
        .unwrap();
        let Some(Sub::SshKey {
            cmd: SshKeyCmd::Add(f),
        }) = a.cmd
        else {
            panic!("not ssh-key add");
        };
        let args = f.into_args();
        assert_eq!(
            (args.user.as_deref(), args.deploy, args.write),
            (Some("alice"), false, false)
        );
        assert_eq!(args.expires.as_deref(), Some("30d"));

        let a = parse(&[
            "ssh-key",
            "add",
            "--repo",
            "acme/web",
            "--deploy",
            "--write",
            "--key-file",
            "k.pub",
        ])
        .unwrap();
        let Some(Sub::SshKey {
            cmd: SshKeyCmd::Add(f),
        }) = a.cmd
        else {
            panic!("not ssh-key add");
        };
        let args = f.into_args();
        assert_eq!(
            (args.repo.as_deref(), args.deploy, args.write),
            (Some("acme/web"), true, true)
        );
        assert!(matches!(
            parse(&["ssh-key", "revoke", "SHA256:abc"]).unwrap().cmd,
            Some(Sub::SshKey {
                cmd: SshKeyCmd::Revoke { .. }
            })
        ));
        assert!(matches!(
            parse(&["ssh-key", "list"]).unwrap().cmd,
            Some(Sub::SshKey {
                cmd: SshKeyCmd::List
            })
        ));
    }

    #[test]
    fn mixed_or_incomplete_flags_are_refused() {
        for bad in [
            &["ssh-key", "add", "--key-file", "k.pub"][..],
            &["ssh-key", "add", "--user", "a"],
            &[
                "ssh-key",
                "add",
                "--user",
                "a",
                "--repo",
                "o/r",
                "--key-file",
                "k",
            ],
            &[
                "ssh-key",
                "add",
                "--user",
                "a",
                "--write",
                "--key-file",
                "k",
            ],
            &[
                "ssh-key",
                "add",
                "--user",
                "a",
                "--deploy",
                "--key-file",
                "k",
            ],
            &["ssh-key", "add", "--repo", "o/r", "--key-file", "k"],
            &["ssh-key", "add", "--deploy", "--key-file", "k"],
            &[
                "ssh-key",
                "add",
                "--repo",
                "o/r",
                "--write",
                "--key-file",
                "k",
            ],
            &["ssh-key", "revoke"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
