//! `asc serve [migrate] [--config <file>] [--role <role>] [--upstream <url>]` (SPEC §8.1).
//!
//! `migrate` applies the schema and exits; the server itself never migrates at start.
//!
//! `asc serve backup --out <dir> [--key-file <f> --include-vault-key] [--no-objects]` writes a backup
//! and `asc serve restore --from <dir> [--key-file <f>] [--force]` restores one (docs/runbooks/backup.md).
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
        /// Writes a consistent backup of the database, the bucket's objects and, sealed, the vault key.
        Backup {
            /// The directory to write; it must not exist or be empty.
            #[arg(long)]
            out: PathBuf,
            /// A 32-byte key file (mode 0600) the vault key is sealed under.
            #[arg(long)]
            key_file: Option<PathBuf>,
            /// Include the vault master key, sealed under `--key-file`.
            #[arg(long, requires = "key_file")]
            include_vault_key: bool,
            /// Leave the bucket's objects out (the bucket is backed up on its own).
            #[arg(long)]
            no_objects: bool,
        },
        /// Restores a backup into an empty install; stop the server first.
        Restore {
            /// The backup directory.
            #[arg(long)]
            from: PathBuf,
            /// The key file the vault key was sealed under.
            #[arg(long)]
            key_file: Option<PathBuf>,
            /// Write over a database, bucket or vault key that already holds data.
            #[arg(long)]
            force: bool,
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
        /// Packed objects: `run` packs old loose objects and merges and compacts packs, `status`
        /// counts, `verify` checks packs against the index, `reindex` rebuilds the index from the
        /// packs.
        Pack {
            #[command(subcommand)]
            cmd: PackSub,
            /// A repository; repeatable.
            #[arg(long = "repo", global = true)]
            repos: Vec<String>,
            /// Every repository.
            #[arg(long, global = true)]
            all: bool,
        },
        /// Delta storage: `run` stores mainline file versions as deltas against their previous
        /// version under the collection lock, `status` counts deltas, bases and the backlog.
        Delta {
            #[command(subcommand)]
            cmd: DeltaSub,
            /// A repository; repeatable.
            #[arg(long = "repo", global = true)]
            repos: Vec<String>,
            /// Every repository.
            #[arg(long, global = true)]
            all: bool,
        },
        /// Read-only maintenance (SPEC §14.5): `on` refuses every write on every node until
        /// `off`; `status` prints the switch.
        Maintenance {
            #[command(subcommand)]
            cmd: MaintenanceSub,
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
    pub enum PackSub {
        /// One maintenance pass.
        Run {
            /// Packs even when fewer than `[pack] min_loose` objects are waiting.
            #[arg(long)]
            force: bool,
        },
        Status,
        Verify,
        Reindex,
    }

    #[derive(clap::Subcommand, Debug)]
    pub enum DeltaSub {
        /// One delta pass, whether or not `[delta] enabled` is set.
        Run,
        Status,
        /// Rewrites every delta as a full value (the rollback of delta storage).
        Expand,
    }

    #[derive(clap::Subcommand, Debug)]
    pub enum MaintenanceSub {
        /// Refuses every write until `off`; reads keep working.
        On {
            /// Shown to everyone who tries to write and on the hub's notice (at most 500
            /// characters).
            #[arg(long, default_value = "")]
            message: String,
            /// The expected length, such as `30m` or `2h`; shown, never acted on.
            #[arg(long = "for", value_parser = parse_for)]
            for_secs: Option<u64>,
            /// The release being installed, shown as "upgrading to <version>".
            #[arg(long, default_value = "")]
            version: String,
        },
        /// Accepts writes again.
        Off,
        /// Prints whether maintenance is on, why and since when.
        Status,
    }

    fn parse_for(text: &str) -> std::result::Result<u64, String> {
        crate::daemon::live::prod::parse_duration(text)
            .map(|d| d.as_secs())
            .filter(|s| *s > 0)
            .ok_or_else(|| "a length such as 90s, 30m or 2h".to_owned())
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
            "--out",
            "--from",
            "--include-vault-key",
            "--no-objects",
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
            AdminCmd::Pack { cmd, repos, all } => {
                let cmd = match cmd {
                    PackSub::Run { force } => ac::PackCmd::Run { force },
                    PackSub::Status => ac::PackCmd::Status,
                    PackSub::Verify => ac::PackCmd::Verify,
                    PackSub::Reindex => ac::PackCmd::Reindex,
                };
                cli.pack(&cmd, &repos, all).await
            }
            AdminCmd::Delta { cmd, repos, all } => {
                let cmd = match cmd {
                    DeltaSub::Run => ac::DeltaCmd::Run,
                    DeltaSub::Status => ac::DeltaCmd::Status,
                    DeltaSub::Expand => ac::DeltaCmd::Expand,
                };
                cli.delta(&cmd, &repos, all).await
            }
            AdminCmd::Maintenance { cmd } => {
                let cmd = match cmd {
                    MaintenanceSub::On {
                        message,
                        for_secs,
                        version,
                    } => ac::MaintenanceCmd::On {
                        message,
                        for_secs,
                        version,
                    },
                    MaintenanceSub::Off => ac::MaintenanceCmd::Off,
                    MaintenanceSub::Status => ac::MaintenanceCmd::Status,
                };
                cli.maintenance(&cmd, asc_serve::ai::clock::now()).await
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
            Some(Sub::Migrate) => {
                asc_serve::logging::init(&cfg.log);
                asc_serve::run_migrate(&cfg).await.map_err(fatal)?;
            }
            Some(Sub::Admin { cmd }) => return admin(&cfg, cmd).await,
            Some(Sub::Backup {
                out,
                key_file,
                include_vault_key,
                no_objects,
            }) => {
                let o = asc_serve::backup::BackupOptions {
                    out: ctx.cwd.join(out),
                    key_file: key_file.map(|k| ctx.cwd.join(k)),
                    include_vault_key,
                    no_objects,
                };
                print!(
                    "{}",
                    asc_serve::backup::backup(&cfg, &o).await.map_err(fatal)?
                );
            }
            Some(Sub::Restore {
                from,
                key_file,
                force,
            }) => {
                let o = asc_serve::backup::RestoreOptions {
                    from: ctx.cwd.join(from),
                    key_file: key_file.map(|k| ctx.cwd.join(k)),
                    force,
                };
                print!(
                    "{}",
                    asc_serve::backup::restore(&cfg, &o).await.map_err(fatal)?
                );
            }
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
                // The server's one subscriber: RUST_LOG, `ASC_LOG_FORMAT` / `[log] format`, stderr.
                asc_serve::logging::init(&cfg.log);
                let authorizer = asc_serve::authz::authorizer(&cfg).await.map_err(fatal)?;
                // Ctrl-C everywhere; SIGTERM too on Unix, which is what `docker stop` sends.
                let stop = async {
                    #[cfg(unix)]
                    {
                        use tokio::signal::unix::{SignalKind, signal};
                        match signal(SignalKind::terminate()) {
                            Ok(mut term) => {
                                tokio::select! {
                                    _ = tokio::signal::ctrl_c() => {}
                                    _ = term.recv() => {}
                                }
                            }
                            Err(_) => {
                                let _ = tokio::signal::ctrl_c().await;
                            }
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = tokio::signal::ctrl_c().await;
                    }
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

#[cfg(all(test, feature = "serve"))]
mod maintenance_tests {
    use clap::Parser as _;

    use super::{AdminCmd, Args, MaintenanceSub, Sub};

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("serve").chain(args.iter().copied()))
    }

    fn sub(args: &[&str]) -> MaintenanceSub {
        let Some(Sub::Admin {
            cmd: AdminCmd::Maintenance { cmd },
        }) = parse(args).unwrap().cmd
        else {
            panic!("not admin maintenance");
        };
        cmd
    }

    #[test]
    fn maintenance_on_off_and_status_parse() {
        let MaintenanceSub::On {
            message,
            for_secs,
            version,
        } = sub(&[
            "admin",
            "maintenance",
            "on",
            "--message",
            "new disks",
            "--for",
            "30m",
            "--version",
            "2.4.0",
        ])
        else {
            panic!("not on");
        };
        assert_eq!(
            (message.as_str(), for_secs, version.as_str()),
            ("new disks", Some(1800), "2.4.0")
        );
        let MaintenanceSub::On {
            message, for_secs, ..
        } = sub(&["admin", "maintenance", "on"])
        else {
            panic!("not on");
        };
        assert!(message.is_empty() && for_secs.is_none());
        assert!(matches!(
            sub(&["admin", "maintenance", "off"]),
            MaintenanceSub::Off
        ));
        assert!(matches!(
            sub(&["admin", "maintenance", "status"]),
            MaintenanceSub::Status
        ));
        for bad in ["0", "soon", "5d"] {
            assert!(
                parse(&["admin", "maintenance", "on", "--for", bad]).is_err(),
                "{bad}"
            );
        }
        assert!(parse(&["admin", "maintenance"]).is_err());
    }
}
