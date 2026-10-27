//! `ServerGit` (SPEC §25.2, §25.8 items 2 and 3): the only way the server layer starts a git
//! child.
//!
//! It can be built only from a [`RealGit`], so "all git interop goes through `asc_git::RealGit`"
//! still holds. A child starts from an **empty environment** and gets only the fixed allow-list of
//! [`ServerGit::child_env`]; every behaviour is passed as `-c`, which outranks the view's own
//! `config` ([`ServerGit::config_args`]); `core.hooksPath` is the asc-owned directory, so no
//! repository-supplied hook, config or filter ever runs. Nothing a client sent is ever an argv
//! element: the arguments are fixed words, numbers from [`GitLimits`] and a path asc derived from
//! a repository id.
//!
//! Each child is supervised ([`ServerChild`]): a Job Object (Windows) or a process group with
//! `rlimit`s (Unix) bounds memory, CPU and process count and lets the whole tree be killed; a
//! watchdog enforces the wall-clock deadline and the idle limit.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use asc_core::id::GitOid;

use super::limits::{GitLimits, LimitError};
use super::pktline::GitProtocol;
use crate::realgit::{GitVersion, MIN_VERSION, RealGit, RealGitError};

/// The `agent=` and HTTP user agent prefix; the git version is never shown.
pub const USER_AGENT_PREFIX: &str = "asc/";
/// Stderr kept from a child; the rest is read and dropped so the child never blocks on it.
pub const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Stdout accepted from an administrative command (`for-each-ref`, `symbolic-ref`, …).
pub const MAX_ADMIN_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Ref updates in one `update-ref --stdin` transaction.
pub const MAX_REF_UPDATES: usize = 500_000;
/// Longest ref name accepted.
pub const MAX_REF_NAME_BYTES: usize = 1024;
/// Largest repository `config` read; a larger file is refused.
pub const MAX_REPO_CONFIG_BYTES: u64 = 1024 * 1024;
/// Bytes read from a pipe per call.
const CHUNK: usize = 64 * 1024;
/// Granularity of the watchdog's sleep when it has nothing else to wait for.
const WATCHDOG_TICK: Duration = Duration::from_secs(1);
/// Wall-clock bound of the `git --version` probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Memory, CPU seconds and processes granted to the probe and to administrative commands.
const ADMIN_MEMORY: u64 = 2 * 1024 * 1024 * 1024;

/// How hard fsck is on pushes (SPEC §25.8 item 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsckMode {
    /// Git's defaults for every check.
    #[default]
    Strict,
    /// The harmless legacy classes of [`MIGRATION_RELAXED`] are ignored, so old histories from
    /// other hosts can be pushed.
    Migration,
}

/// The fsck message ids that `fsck = "migration"` downgrades with `receive.fsck.<id>=ignore`.
/// All are malformed-header classes of old histories. The dangerous classes (`hasDotgit`,
/// `gitmodules*`, `nulInHeader`, `nulInCommit`, `badTree`, …) are never in this list.
pub const MIGRATION_RELAXED: &[&str] = &[
    "zeroPaddedFilemode",
    "badTimezone",
    "missingEmail",
    "badEmail",
    "badDate",
    "badDateOverflow",
    "badName",
    "emptyName",
    "missingSpaceBeforeDate",
    "missingSpaceBeforeEmail",
    "missingNameBeforeEmail",
];

/// Why a server-side git operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ServerGitError {
    #[error(transparent)]
    Real(#[from] RealGitError),
    #[error(transparent)]
    Limits(#[from] LimitError),
    #[error("server git: {0}")]
    Config(&'static str),
    #[error("git {found} is older than the required {}.{}", MIN_VERSION.major, MIN_VERSION.minor)]
    TooOld { found: String },
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error("git {command} failed ({status}): {stderr}")]
    Failed {
        command: &'static str,
        status: String,
        stderr: String,
    },
    #[error("git {command} was killed: {reason}")]
    Killed {
        command: &'static str,
        reason: KillReason,
    },
    #[error("git {command} wrote more than {MAX_ADMIN_OUTPUT_BYTES} bytes")]
    OutputTooLarge { command: &'static str },
    #[error("git {command} printed something unexpected: {what}")]
    BadOutput {
        command: &'static str,
        what: &'static str,
    },
    #[error("ref name {0:?} is not acceptable")]
    BadRefName(String),
    #[error("{0} ref updates exceed the limit of {MAX_REF_UPDATES}")]
    TooManyRefUpdates(usize),
    #[error("path {0:?} must be absolute")]
    NotAbsolute(PathBuf),
    #[error("the repository config has [{section}] {key}, which a served repository may not have")]
    UntrustedConfig { section: String, key: String },
    #[error("the repository config is invalid or larger than {MAX_REPO_CONFIG_BYTES} bytes")]
    UnreadableConfig,
}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> ServerGitError {
    move |source| ServerGitError::Io { what, source }
}

/// Configuration of [`ServerGit`].
#[derive(Debug, Clone)]
pub struct ServerGitConfig {
    /// The asc-owned hooks directory (`core.hooksPath`). It is created empty when missing.
    pub hooks_dir: PathBuf,
    /// An asc-owned private directory for the empty `HOME` and the empty config file.
    pub state_dir: PathBuf,
    pub limits: GitLimits,
    pub fsck: FsckMode,
}

/// Which pack service a child runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackService {
    UploadPack,
    ReceivePack,
}

/// Parameters of a pack child.
#[derive(Debug, Clone, Default)]
pub struct PackRequest {
    /// `--advertise-refs`: write the advertisement and exit.
    pub advertise_refs: bool,
    /// The validated `Git-Protocol` value; `None` is protocol v0.
    pub protocol: Option<GitProtocol>,
}

/// What a push's quarantine holds ([`ServerGit::quarantine_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuarantineStats {
    /// Objects in the quarantine; `u64::MAX` when the listing was too long to read.
    pub objects: u64,
    /// The inflated size of the largest one.
    pub largest: u64,
}

/// Whether `name` may be added to a pack child's environment: `ASC_` and upper-case ASCII, digits
/// and `_` only.
fn valid_extra_env_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with("ASC_")
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// One ref change for [`ServerGit::update_refs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdate {
    Set { name: String, oid: GitOid },
    Delete { name: String },
}

impl RefUpdate {
    fn name(&self) -> &str {
        match self {
            Self::Set { name, .. } | Self::Delete { name } => name,
        }
    }
}

/// Whether `name` is a ref name git accepts and `update-ref --stdin -z` can carry unambiguously
/// (the rules of `git check-ref-format`, plus no control byte, no `\`, at most
/// [`MAX_REF_NAME_BYTES`]).
pub fn valid_ref_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_REF_NAME_BYTES
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name == "@"
    {
        return false;
    }
    if name
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b" ~^:?*[\\".contains(&b))
    {
        return false;
    }
    name.split('/')
        .all(|c| !c.starts_with('.') && !c.ends_with(".lock"))
}

/// Sections and keys a served repository's `config` may hold (compared case-insensitively):
/// exactly what `git init --bare` and `git clone --mirror` write. `-c` outranks any key set
/// here, but a few keys have no `-c` override (`receive.certNonceSeed` cannot be unset, and
/// `include.path` pulls in a second file), so a `config` outside this list is refused outright
/// instead of trusted to be overridden.
const REPO_CONFIG_ALLOWED: &[(&str, &[&str])] = &[
    (
        "core",
        &[
            "repositoryformatversion",
            "filemode",
            "bare",
            "logallrefupdates",
            "ignorecase",
            "precomposeunicode",
            "symlinks",
            "longpaths",
            "protectntfs",
            "protecthfs",
        ],
    ),
    ("remote", &["url", "fetch", "mirror", "tagopt", "prune"]),
];

/// Refuses a repository whose `config` has a section or key outside the allow-list. Run before
/// every child that reads the repository.
pub fn check_repo_config(dir: &Path) -> Result<(), ServerGitError> {
    let file = match fs::File::open(dir.join("config")) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err("reading the repository config")(e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_REPO_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err("reading the repository config"))?;
    if bytes.len() as u64 > MAX_REPO_CONFIG_BYTES {
        return Err(ServerGitError::UnreadableConfig);
    }
    let parsed = gix_config::File::from_bytes_no_includes(
        &bytes,
        gix_config::file::Metadata::api(),
        gix_config::file::init::Options::default(),
    )
    .map_err(|_| ServerGitError::UnreadableConfig)?;
    for section in parsed.sections() {
        let name = section.header().name().to_string().to_ascii_lowercase();
        let Some((_, keys)) = REPO_CONFIG_ALLOWED.iter().find(|(s, _)| *s == name) else {
            return Err(ServerGitError::UntrustedConfig {
                section: name,
                key: String::new(),
            });
        };
        for key in section.value_names() {
            if !keys.contains(&key.to_ascii_lowercase().as_str()) {
                return Err(ServerGitError::UntrustedConfig { section: name, key });
            }
        }
    }
    Ok(())
}

/// Starts supervised git children with a fixed environment and fixed configuration.
#[derive(Clone)]
pub struct ServerGit {
    inner: Arc<Inner>,
}

struct Inner {
    real: RealGit,
    hooks_dir: PathBuf,
    home: PathBuf,
    empty_config: PathBuf,
    path_env: OsString,
    limits: GitLimits,
    fsck: FsckMode,
    version: GitVersion,
}

impl std::fmt::Debug for ServerGit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerGit")
            .field("git", &self.inner.real.path())
            .field("version", &self.inner.version)
            .finish_non_exhaustive()
    }
}

impl ServerGit {
    /// Prepares the private directories and checks `git --version` (at least 2.40). The only
    /// constructor: a `ServerGit` always wraps a resolved [`RealGit`].
    pub fn new(git: &RealGit, cfg: ServerGitConfig) -> Result<Self, ServerGitError> {
        cfg.limits.validate()?;
        for p in [&cfg.hooks_dir, &cfg.state_dir] {
            if !p.is_absolute() {
                return Err(ServerGitError::NotAbsolute(p.clone()));
            }
        }
        fs::create_dir_all(&cfg.hooks_dir).map_err(io_err("creating the hooks directory"))?;
        let home = cfg.state_dir.join("home");
        // An empty private HOME: whatever a previous run left is removed.
        match fs::remove_dir_all(&home) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("clearing the private HOME")(e)),
        }
        private_dir(&home).map_err(io_err("creating the private HOME"))?;
        let empty_config = cfg.state_dir.join("empty.gitconfig");
        fs::write(&empty_config, b"").map_err(io_err("writing the empty config file"))?;
        let mut this = Self {
            inner: Arc::new(Inner {
                real: git.clone(),
                hooks_dir: cfg.hooks_dir,
                home,
                empty_config,
                path_env: path_env(git),
                limits: cfg.limits,
                fsck: cfg.fsck,
                version: MIN_VERSION,
            }),
        };
        let out = this.run(
            "version",
            this.command(&["--version"], None, false)?,
            b"",
            &ChildLimits::probe(),
        )?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let found = GitVersion::parse(&text).filter(|v| *v >= MIN_VERSION);
        let Some(version) = found else {
            return Err(ServerGitError::TooOld {
                found: text.trim().to_owned(),
            });
        };
        if let Some(inner) = Arc::get_mut(&mut this.inner) {
            inner.version = version;
        }
        Ok(this)
    }

    /// The version `git --version` reported.
    pub fn version(&self) -> GitVersion {
        self.inner.version
    }

    pub fn limits(&self) -> &GitLimits {
        &self.inner.limits
    }

    pub fn hooks_dir(&self) -> &Path {
        &self.inner.hooks_dir
    }

    pub fn fsck(&self) -> FsckMode {
        self.inner.fsck
    }

    /// The agent string git advertises: `asc/<semver>`.
    pub fn user_agent() -> String {
        format!("{USER_AGENT_PREFIX}{}", env!("CARGO_PKG_VERSION"))
    }

    /// The whole environment of every child, and nothing else (SPEC §25.8 item 2).
    pub fn child_env(&self, protocol: Option<&GitProtocol>) -> Vec<(OsString, OsString)> {
        let inner = &*self.inner;
        let mut env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), inner.path_env.clone()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                inner.empty_config.clone().into(),
            ),
            (
                "GIT_CONFIG_SYSTEM".into(),
                inner.empty_config.clone().into(),
            ),
            ("HOME".into(), inner.home.clone().into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
            ("GIT_USER_AGENT".into(), Self::user_agent().into()),
            ("LC_ALL".into(), "C".into()),
        ];
        if let Some(p) = protocol {
            // Already validated by `GitProtocol::parse`.
            env.push(("GIT_PROTOCOL".into(), p.as_str().into()));
        }
        if cfg!(windows) {
            let root = std::env::var_os("SystemRoot")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| OsString::from("C:\\Windows"));
            env.push(("SystemRoot".into(), root));
        }
        env
    }

    /// Every `-c key=value` pair passed to every child (SPEC §25.8 item 2). They outrank the
    /// repository's own `config`. Keys that may repeat (`transfer.hideRefs`) repeat.
    pub fn config_args(&self) -> Vec<(String, OsString)> {
        let l = &self.inner.limits;
        let s = |v: &str| OsString::from(v);
        let mut c: Vec<(String, OsString)> = vec![
            (
                "core.hooksPath".into(),
                git_path_value(&self.inner.hooks_dir),
            ),
            ("protocol.allow".into(), s("never")),
            ("core.fsmonitor".into(), s("false")),
            ("core.sshCommand".into(), s("false")),
            ("gc.auto".into(), s("0")),
            ("receive.autogc".into(), s("false")),
            ("receive.denyDeletes".into(), s("false")),
            ("receive.denyNonFastForwards".into(), s("false")),
            ("receive.fsckObjects".into(), s("true")),
            ("transfer.fsckObjects".into(), s("true")),
            (
                "receive.maxInputSize".into(),
                l.max_push_bytes.to_string().into(),
            ),
            ("receive.unpackLimit".into(), s("1")),
            ("receive.advertisePushOptions".into(), s("true")),
            ("receive.advertiseAtomic".into(), s("true")),
            ("receive.updateServerInfo".into(), s("false")),
            ("uploadpack.allowFilter".into(), s("true")),
            ("uploadpack.allowAnySHA1InWant".into(), s("false")),
            ("uploadpack.allowReachableSHA1InWant".into(), s("false")),
            ("transfer.hideRefs".into(), s("refs/pull")),
            ("transfer.hideRefs".into(), s("refs/users")),
            ("transfer.hideRefs".into(), s("refs/snapshots")),
            ("pack.threads".into(), l.pack_threads.to_string().into()),
        ];
        if self.inner.fsck == FsckMode::Migration {
            c.extend(
                MIGRATION_RELAXED
                    .iter()
                    .map(|id| (format!("receive.fsck.{id}"), s("ignore"))),
            );
        }
        c
    }

    /// A command for `args` (the subcommand and its fixed words) with the empty environment, the
    /// `-c` configuration and the private HOME as working directory.
    fn command(
        &self,
        args: &[&str],
        protocol: Option<&GitProtocol>,
        with_config: bool,
    ) -> Result<Command, ServerGitError> {
        let mut cmd = self.inner.real.command()?;
        cmd.env_clear();
        for (k, v) in self.child_env(protocol) {
            cmd.env(k, v);
        }
        if with_config {
            for (k, v) in self.config_args() {
                let mut kv = OsString::from(format!("{k}="));
                kv.push(v);
                cmd.arg("-c").arg(kv);
            }
        }
        cmd.args(args);
        // Never the server's own working directory: it could hold a repository git would find.
        cmd.current_dir(&self.inner.home);
        Ok(cmd)
    }

    /// Starts `git upload-pack` or `git receive-pack` in `--stateless-rpc` mode on `view`.
    /// The caller streams the HTTP body into `stdin` and `stdout` back, calling
    /// [`Activity::touch`] as bytes move, and ends with [`ServerChild::wait`].
    pub fn pack(
        &self,
        service: PackService,
        view: &Path,
        req: &PackRequest,
    ) -> Result<ServerChild, ServerGitError> {
        self.pack_with_env(service, view, req, &[])
    }

    /// [`ServerGit::pack`] with extra environment variables for the child and for the hook it
    /// starts (git passes its environment on to `pre-receive`). Only names of the form
    /// `ASC_[A-Z0-9_]+` are accepted, so a caller can never reach git's own `GIT_*` switches
    /// (`GIT_CONFIG_COUNT`, `GIT_DIR`, `GIT_EXEC_PATH`, …) through this door.
    pub fn pack_with_env(
        &self,
        service: PackService,
        view: &Path,
        req: &PackRequest,
        env: &[(&str, &str)],
    ) -> Result<ServerChild, ServerGitError> {
        self.pack_child(service, view, req, env, false, &[])
    }

    /// [`ServerGit::pack_with_env`] that also hides `hidden` ref names from an `upload-pack`
    /// (`transfer.hideRefs`): refs that exist in the view for a moment but are not shared (a
    /// push stored as a user branch, before the view is synced). Each name must be a full
    /// `refs/...` name; a hidden ref is neither advertised nor wantable.
    pub fn pack_hiding(
        &self,
        service: PackService,
        view: &Path,
        req: &PackRequest,
        env: &[(&str, &str)],
        hidden: &[String],
    ) -> Result<ServerChild, ServerGitError> {
        self.pack_child(service, view, req, env, false, hidden)
    }

    /// The **session mode** of SPEC §25.11.5: `git upload-pack <view>` or `git receive-pack <view>`
    /// without `--stateless-rpc`, for a transport that keeps one full-duplex stream open (SSH).
    /// The child writes its own advertisement first and then reads the client's commands, so
    /// `advertise_refs` has no meaning here and is refused. The environment, every `-c`, the hook
    /// directory, the child limits and the process-tree kill are those of
    /// [`ServerGit::pack_with_env`]; `env` takes only `ASC_*` names.
    pub fn session_with_env(
        &self,
        service: PackService,
        view: &Path,
        protocol: Option<&GitProtocol>,
        env: &[(&str, &str)],
    ) -> Result<ServerChild, ServerGitError> {
        let req = PackRequest {
            advertise_refs: false,
            protocol: protocol.cloned(),
        };
        self.pack_child(service, view, &req, env, true, &[])
    }

    /// [`ServerGit::session_with_env`] that also hides `hidden` refs ([`ServerGit::pack_hiding`]).
    pub fn session_hiding(
        &self,
        service: PackService,
        view: &Path,
        protocol: Option<&GitProtocol>,
        env: &[(&str, &str)],
        hidden: &[String],
    ) -> Result<ServerChild, ServerGitError> {
        let req = PackRequest {
            advertise_refs: false,
            protocol: protocol.cloned(),
        };
        self.pack_child(service, view, &req, env, true, hidden)
    }

    fn pack_child(
        &self,
        service: PackService,
        view: &Path,
        req: &PackRequest,
        env: &[(&str, &str)],
        session: bool,
        hidden: &[String],
    ) -> Result<ServerChild, ServerGitError> {
        for (name, value) in env {
            if !valid_extra_env_name(name) || value.contains('\0') {
                return Err(ServerGitError::Config(
                    "an extra environment name is not allowed",
                ));
            }
        }
        require_absolute(view)?;
        check_repo_config(view)?;
        let l = &self.inner.limits;
        if hidden
            .iter()
            .any(|n| !n.starts_with("refs/") || !valid_ref_name(n))
        {
            return Err(ServerGitError::Config("a hidden ref name is not allowed"));
        }
        let mut cmd = self.command(&[], req.protocol.as_ref(), true)?;
        match service {
            PackService::UploadPack => {
                for name in hidden {
                    cmd.arg("-c").arg(format!("transfer.hideRefs={name}"));
                }
                cmd.arg("upload-pack");
                if !session {
                    cmd.arg("--stateless-rpc");
                }
                cmd.arg("--strict");
                cmd.arg(format!("--timeout={}", l.idle_timeout.as_secs().max(1)));
            }
            PackService::ReceivePack => {
                cmd.arg("receive-pack");
                if !session {
                    cmd.arg("--stateless-rpc");
                }
            }
        }
        if req.advertise_refs {
            cmd.arg("--advertise-refs");
        }
        cmd.arg(view);
        for (name, value) in env {
            cmd.env(name, value);
        }
        spawn_supervised(cmd, &ChildLimits::request(l), true)
    }

    /// Counts the objects of a push's quarantine and finds the largest, without reading any
    /// content into this process: `git cat-file --batch-all-objects --batch-check` on the
    /// quarantine directory alone (SPEC §25.5: `max_push_objects` and `max_object_bytes` are
    /// checked from the quarantine before import). A listing longer than
    /// [`MAX_ADMIN_OUTPUT_BYTES`] is reported as more objects than any limit allows.
    pub fn quarantine_stats(
        &self,
        view: &Path,
        quarantine: &Path,
    ) -> Result<QuarantineStats, ServerGitError> {
        require_absolute(quarantine)?;
        let mut cmd = self.view_command(
            view,
            &[
                "cat-file",
                "--batch-all-objects",
                "--unordered",
                "--batch-check=%(objectsize)",
            ],
        )?;
        // Only the quarantine: the view's own objects are not part of the push.
        cmd.env("GIT_OBJECT_DIRECTORY", quarantine);
        let out = match self.run(
            "cat-file",
            cmd,
            b"",
            &ChildLimits::admin(&self.inner.limits),
        ) {
            Ok(out) => out,
            Err(ServerGitError::OutputTooLarge { .. }) => {
                return Ok(QuarantineStats {
                    objects: u64::MAX,
                    largest: 0,
                });
            }
            Err(e) => return Err(e),
        };
        let bad = |what| ServerGitError::BadOutput {
            command: "cat-file",
            what,
        };
        let text = std::str::from_utf8(&out.stdout).map_err(|_| bad("not UTF-8"))?;
        let mut stats = QuarantineStats::default();
        for line in text.lines() {
            let size: u64 = line
                .trim()
                .parse()
                .map_err(|_| bad("a size that is not a number"))?;
            stats.objects += 1;
            stats.largest = stats.largest.max(size);
        }
        Ok(stats)
    }

    /// `git init --bare --template=` (an empty template: no sample hooks, no `info/exclude`).
    pub fn init_bare(&self, dir: &Path) -> Result<(), ServerGitError> {
        require_absolute(dir)?;
        let mut cmd = self.command(&[], None, true)?;
        cmd.args([
            "-c",
            "init.defaultBranch=master",
            "init",
            "--bare",
            "--quiet",
            "--template=",
        ]);
        cmd.arg(dir);
        self.run("init", cmd, b"", &ChildLimits::admin(&self.inner.limits))?;
        Ok(())
    }

    /// Applies ref changes in one transaction (`update-ref --stdin -z`).
    pub fn update_refs(&self, view: &Path, updates: &[RefUpdate]) -> Result<(), ServerGitError> {
        if updates.is_empty() {
            return Ok(());
        }
        if updates.len() > MAX_REF_UPDATES {
            return Err(ServerGitError::TooManyRefUpdates(updates.len()));
        }
        let mut input = Vec::new();
        for u in updates {
            if !valid_ref_name(u.name()) {
                return Err(ServerGitError::BadRefName(u.name().to_owned()));
            }
            match u {
                // `update SP <ref> NUL <new> NUL <old> NUL`; an empty old value skips the check.
                RefUpdate::Set { name, oid } => {
                    input.extend_from_slice(format!("update {name}\0{oid}\0\0").as_bytes());
                }
                // `delete SP <ref> NUL <old> NUL`.
                RefUpdate::Delete { name } => {
                    input.extend_from_slice(format!("delete {name}\0\0").as_bytes());
                }
            }
        }
        let cmd = self.view_command(view, &["update-ref", "--stdin", "-z"])?;
        self.run(
            "update-ref",
            cmd,
            &input,
            &ChildLimits::admin(&self.inner.limits),
        )?;
        Ok(())
    }

    /// Every ref of the view with its object id.
    pub fn list_refs(&self, view: &Path) -> Result<Vec<(String, GitOid)>, ServerGitError> {
        let cmd = self.view_command(
            view,
            &["for-each-ref", "--format=%(objectname) %(refname)", "refs/"],
        )?;
        let out = self.run(
            "for-each-ref",
            cmd,
            b"",
            &ChildLimits::admin(&self.inner.limits),
        )?;
        let bad = |what| ServerGitError::BadOutput {
            command: "for-each-ref",
            what,
        };
        let text = std::str::from_utf8(&out.stdout).map_err(|_| bad("not UTF-8"))?;
        let mut refs = Vec::new();
        for line in text.lines() {
            let (oid, name) = line
                .split_once(' ')
                .ok_or_else(|| bad("a line without a space"))?;
            let oid: GitOid = oid
                .parse()
                .map_err(|_| bad("an object id that is not SHA-1"))?;
            refs.push((name.to_owned(), oid));
        }
        Ok(refs)
    }

    /// The branch `HEAD` names, e.g. `refs/heads/master`.
    pub fn read_head(&self, view: &Path) -> Result<Option<String>, ServerGitError> {
        let cmd = self.view_command(view, &["symbolic-ref", "-q", "HEAD"])?;
        let out =
            self.run_allow_failure("symbolic-ref", cmd, &ChildLimits::admin(&self.inner.limits))?;
        if !out.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty()))
    }

    /// Points `HEAD` at `branch_ref` (which need not exist yet).
    pub fn set_head(&self, view: &Path, branch_ref: &str) -> Result<(), ServerGitError> {
        if !valid_ref_name(branch_ref) || !branch_ref.starts_with("refs/heads/") {
            return Err(ServerGitError::BadRefName(branch_ref.to_owned()));
        }
        let cmd = self.view_command(view, &["symbolic-ref", "HEAD", branch_ref])?;
        self.run(
            "symbolic-ref",
            cmd,
            b"",
            &ChildLimits::admin(&self.inner.limits),
        )?;
        Ok(())
    }

    /// `git --git-dir <view> <args…>` with the fixed environment and configuration.
    fn view_command(&self, view: &Path, args: &[&str]) -> Result<Command, ServerGitError> {
        require_absolute(view)?;
        check_repo_config(view)?;
        let mut cmd = self.command(&[], None, true)?;
        cmd.arg("--git-dir").arg(view).args(args);
        Ok(cmd)
    }

    fn run(
        &self,
        command: &'static str,
        cmd: Command,
        input: &[u8],
        limits: &ChildLimits,
    ) -> Result<Captured, ServerGitError> {
        let out = self.run_allow_failure_with(command, cmd, input, limits)?;
        if let Some(reason) = out.killed {
            return Err(ServerGitError::Killed { command, reason });
        }
        if !out.status.success() {
            return Err(ServerGitError::Failed {
                command,
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        Ok(out)
    }

    fn run_allow_failure(
        &self,
        command: &'static str,
        cmd: Command,
        limits: &ChildLimits,
    ) -> Result<Captured, ServerGitError> {
        self.run_allow_failure_with(command, cmd, b"", limits)
    }

    fn run_allow_failure_with(
        &self,
        command: &'static str,
        cmd: Command,
        input: &[u8],
        limits: &ChildLimits,
    ) -> Result<Captured, ServerGitError> {
        run_captured(cmd, input, limits).map_err(|e| match e {
            CaptureError::Io(source) => ServerGitError::Io {
                what: "running git",
                source,
            },
            CaptureError::TooLarge => ServerGitError::OutputTooLarge { command },
        })
    }
}

fn require_absolute(p: &Path) -> Result<(), ServerGitError> {
    if p.is_absolute() {
        Ok(())
    } else {
        Err(ServerGitError::NotAbsolute(p.to_owned()))
    }
}

/// A path as a git config value: forward slashes, which git accepts on every platform and which
/// need no escaping on the command line.
fn git_path_value(p: &Path) -> OsString {
    if cfg!(windows) {
        OsString::from(p.to_string_lossy().replace('\\', "/"))
    } else {
        p.as_os_str().to_owned()
    }
}

/// `PATH` of every child: the directory of the resolved git, for Git for Windows its
/// `mingw64/bin` and `usr/bin` (which hold the `sh` that runs hooks), and on Unix `/usr/bin` and
/// `/bin`.
fn path_env(git: &RealGit) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = git.path().parent() {
        dirs.push(dir.to_owned());
    }
    if let Some(root) = git.git_for_windows_root() {
        dirs.push(root.join("mingw64").join("bin"));
        dirs.push(root.join("usr").join("bin"));
    }
    // Hooks are shell scripts: they need the system utilities (`cat`, `env`, `sleep`, `awk`) even
    // when git itself sits alone in its directory, as the pinned build does.
    #[cfg(unix)]
    dirs.extend(["/usr/bin", "/bin"].map(PathBuf::from));
    std::env::join_paths(dirs).unwrap_or_default()
}

/// Creates `dir` readable by the owner only (Unix mode 0700).
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

// ------------------------------------------------------------------------------ supervision

/// Why a supervised child was killed by asc (as opposed to ending on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// The wall-clock deadline (`request_timeout`) passed.
    Deadline,
    /// No byte moved either way for `idle_timeout`.
    Idle,
    /// [`ServerChild::kill`] or a drop.
    Requested,
}

impl std::fmt::Display for KillReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Deadline => "the request deadline passed",
            Self::Idle => "no data moved within the idle limit",
            Self::Requested => "it was asked to stop",
        })
    }
}

impl KillReason {
    fn code(self) -> u8 {
        match self {
            Self::Deadline => 1,
            Self::Idle => 2,
            Self::Requested => 3,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(Self::Deadline),
            2 => Some(Self::Idle),
            3 => Some(Self::Requested),
            _ => None,
        }
    }
}

/// Bounds of one child.
#[derive(Debug, Clone)]
pub(crate) struct ChildLimits {
    pub memory: u64,
    pub cpu: Duration,
    pub processes: u32,
    pub deadline: Duration,
    pub idle: Duration,
}

impl ChildLimits {
    /// A pack child: all of `[limits.git]`.
    fn request(l: &GitLimits) -> Self {
        Self {
            memory: l.child_memory,
            cpu: l.request_timeout,
            processes: l.child_processes,
            deadline: l.request_timeout,
            idle: l.idle_timeout,
        }
    }

    /// An administrative command (ref listing and updates): the same memory and process caps,
    /// the request deadline as its time.
    fn admin(l: &GitLimits) -> Self {
        Self::request(l)
    }

    fn probe() -> Self {
        Self {
            memory: ADMIN_MEMORY,
            cpu: PROBE_TIMEOUT,
            processes: super::limits::DEFAULT_CHILD_PROCESSES,
            deadline: PROBE_TIMEOUT,
            idle: PROBE_TIMEOUT,
        }
    }
}

struct Shared {
    start: Instant,
    last_activity_ms: AtomicU64,
    reason: AtomicU8,
    done: Mutex<bool>,
    wake: Condvar,
}

/// Tells the idle watchdog that bytes moved. Cheap to clone and to call.
#[derive(Clone)]
pub struct Activity(Arc<Shared>);

impl Activity {
    pub fn touch(&self) {
        let ms = u64::try_from(self.0.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.0.last_activity_ms.fetch_max(ms, Ordering::Relaxed);
    }
}

/// What a finished child left behind.
#[derive(Debug)]
pub struct ChildExit {
    pub status: ExitStatus,
    /// Set when asc killed the child (deadline, idle limit or request); a child killed by the OS
    /// limits (memory, CPU) simply has a failing `status`.
    pub killed: Option<KillReason>,
    /// The first [`MAX_STDERR_BYTES`] of stderr.
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
}

/// A running, supervised git child.
pub struct ServerChild {
    child: Option<Child>,
    group: Arc<os::Group>,
    shared: Arc<Shared>,
    watchdog: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    /// Write the request body here, then drop it (or call [`ServerChild::close_stdin`]).
    pub stdin: Option<ChildStdin>,
    /// Read the response from here.
    pub stdout: Option<ChildStdout>,
}

pub(crate) fn spawn_supervised(
    mut cmd: Command,
    limits: &ChildLimits,
    piped_stdin: bool,
) -> Result<ServerChild, ServerGitError> {
    cmd.stdin(if piped_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let (mut child, group) = os::spawn(cmd, limits).map_err(io_err("starting git"))?;
    let group = Arc::new(group);
    let shared = Arc::new(Shared {
        start: Instant::now(),
        last_activity_ms: AtomicU64::new(0),
        reason: AtomicU8::new(0),
        done: Mutex::new(false),
        wake: Condvar::new(),
    });
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let watchdog = {
        let (shared, group) = (shared.clone(), group.clone());
        let (deadline, idle) = (limits.deadline, limits.idle);
        std::thread::Builder::new()
            .name("asc-git-watchdog".into())
            .spawn(move || watch(&shared, &group, deadline, idle))
            .map_err(io_err("starting the watchdog"))?
    };
    let stderr = stderr.map(|e| {
        let act = Activity(shared.clone());
        std::thread::spawn(move || drain_stderr(e, &act))
    });
    Ok(ServerChild {
        child: Some(child),
        group,
        shared,
        watchdog: Some(watchdog),
        stderr,
        stdin,
        stdout,
    })
}

fn drain_stderr(mut e: ChildStderr, act: &Activity) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; CHUNK];
    loop {
        match e.read(&mut buf) {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                act.touch();
                let room = MAX_STDERR_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

fn watch(shared: &Shared, group: &os::Group, deadline: Duration, idle: Duration) {
    let mut done = shared.done.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if *done {
            return;
        }
        let now = shared.start.elapsed();
        if now >= deadline {
            shared
                .reason
                .store(KillReason::Deadline.code(), Ordering::SeqCst);
            group.kill();
            return;
        }
        let last = Duration::from_millis(shared.last_activity_ms.load(Ordering::Relaxed));
        let idle_for = now.saturating_sub(last);
        if idle_for >= idle {
            shared
                .reason
                .store(KillReason::Idle.code(), Ordering::SeqCst);
            group.kill();
            return;
        }
        let wait = (deadline - now).min(idle - idle_for).min(WATCHDOG_TICK);
        done = shared
            .wake
            .wait_timeout(done, wait)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

impl ServerChild {
    /// The operating system's id of the child process.
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// The handle for [`Activity::touch`].
    pub fn activity(&self) -> Activity {
        Activity(self.shared.clone())
    }

    /// Closes the child's stdin.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Kills the whole process tree now.
    pub fn kill(&self) {
        let _ = self.shared.reason.compare_exchange(
            0,
            KillReason::Requested.code(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        self.group.kill();
    }

    fn stop_watchdog(&mut self) {
        *self
            .shared
            .done
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.watchdog.take() {
            let _ = h.join();
        }
    }

    /// Waits for the child, then kills anything it left running and collects stderr. Read
    /// `stdout` to its end first (or drop it): a child blocked on a full pipe ends only by the
    /// idle limit.
    pub fn wait(mut self) -> io::Result<ChildExit> {
        self.stdin = None;
        let Some(mut child) = self.child.take() else {
            return Err(io::Error::other("child already waited for"));
        };
        let status = child.wait();
        self.stop_watchdog();
        // Anything still running in the tree was abandoned by the child.
        self.group.kill();
        let stderr = self
            .stderr
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        Ok(ChildExit {
            status: status?,
            killed: KillReason::from_code(self.shared.reason.load(Ordering::SeqCst)),
            stderr,
            elapsed: self.shared.start.elapsed(),
        })
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.group.kill();
            let _ = child.wait();
        }
        self.stop_watchdog();
    }
}

#[derive(Debug)]
pub(crate) struct Captured {
    pub status: ExitStatus,
    pub killed: Option<KillReason>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum CaptureError {
    Io(io::Error),
    TooLarge,
}

/// Runs a command to completion with `input` on stdin, capturing at most
/// [`MAX_ADMIN_OUTPUT_BYTES`] of stdout.
pub(crate) fn run_captured(
    cmd: Command,
    input: &[u8],
    limits: &ChildLimits,
) -> Result<Captured, CaptureError> {
    let mut child = spawn_supervised(cmd, limits, true).map_err(|e| match e {
        ServerGitError::Io { source, .. } => CaptureError::Io(source),
        other => CaptureError::Io(io::Error::other(other.to_string())),
    })?;
    let act = child.activity();
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut out = Vec::new();
    let mut too_large = false;
    std::thread::scope(|s| {
        let writer = stdin.map(|mut si| {
            let act = act.clone();
            s.spawn(move || {
                for part in input.chunks(CHUNK) {
                    if si.write_all(part).is_err() {
                        // A failing git closes the pipe early; its exit status says why.
                        break;
                    }
                    act.touch();
                }
            })
        });
        if let Some(so) = stdout.as_mut() {
            let mut buf = [0u8; CHUNK];
            loop {
                match so.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        act.touch();
                        if out.len() + n > MAX_ADMIN_OUTPUT_BYTES {
                            too_large = true;
                            child.kill();
                            break;
                        }
                        out.extend_from_slice(&buf[..n]);
                    }
                }
            }
        }
        if let Some(w) = writer {
            let _ = w.join();
        }
    });
    drop(stdout);
    let exit = child.wait().map_err(CaptureError::Io)?;
    if too_large {
        return Err(CaptureError::TooLarge);
    }
    Ok(Captured {
        status: exit.status,
        killed: exit.killed,
        stdout: out,
        stderr: exit.stderr,
    })
}

// ------------------------------------------------------------------------------------- Unix

#[cfg(unix)]
#[allow(unsafe_code)] // `pre_exec` runs `setrlimit` between fork and exec; nothing else is unsafe.
mod os {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    use rustix::process::{Pid, Resource, Rlimit, Signal, kill_process_group, setrlimit};

    use super::ChildLimits;

    /// A process group led by the child.
    pub struct Group {
        pgid: u32,
    }

    /// The `RLIMIT_NPROC` value that lets one child tree fork `processes` more tasks.
    ///
    /// `RLIMIT_NPROC` is per real UID, not per process tree: on Linux the kernel compares the
    /// number of tasks (threads included) owned by the UID, across every process of that UID,
    /// with the limit. The server's own tokio and blocking threads therefore count, and an
    /// absolute limit of `processes` (64) is already exceeded before git is spawned, so every
    /// `fork` fails with `EAGAIN`. The cap is relative instead: the UID's tasks at spawn time
    /// plus `processes`, and at least `current + 1` so that the child itself can start. A
    /// fork bomb in a hook or a pack helper still hits the cap after `processes` tasks.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn relative_nproc(current: u64, processes: u64) -> u64 {
        current.saturating_add(processes.max(1))
    }

    /// Tasks (threads included) owned by the real UID, from `/proc`. Fails closed: when the
    /// count is unknown no cap can be derived, so the spawn is refused.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn uid_task_count() -> io::Result<u64> {
        use std::os::unix::fs::MetadataExt;

        let uid = rustix::process::getuid().as_raw();
        let mut tasks = 0u64;
        for entry in std::fs::read_dir("/proc")? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            if !name
                .to_str()
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            // A process that exits during the scan is simply gone.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.uid() != uid {
                continue;
            }
            if let Ok(threads) = std::fs::read_dir(entry.path().join("task")) {
                tasks += threads.filter(Result::is_ok).count() as u64;
            }
        }
        if tasks == 0 {
            return Err(io::Error::other("cannot count the tasks of this user"));
        }
        Ok(tasks)
    }

    pub fn spawn(mut cmd: Command, limits: &ChildLimits) -> io::Result<(Child, Group)> {
        cmd.process_group(0);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let processes = {
            // Counted here, before the fork: `pre_exec` must not allocate or read files.
            let wanted = relative_nproc(uid_task_count()?, u64::from(limits.processes));
            // A hard limit cannot be raised without privilege: stay within the one in force.
            rustix::process::getrlimit(Resource::Nproc)
                .maximum
                .map_or(wanted, |hard| wanted.min(hard))
        };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let processes = u64::from(limits.processes);
        let (memory, cpu_secs) = (limits.memory, limits.cpu.as_secs().max(1));
        // SAFETY: the closure only calls `setrlimit`, an async-signal-safe system call, and
        // allocates nothing.
        unsafe {
            cmd.pre_exec(move || {
                let set = |r: Resource, v: u64| {
                    setrlimit(
                        r,
                        Rlimit {
                            current: Some(v),
                            maximum: Some(v),
                        },
                    )
                    .map_err(io::Error::from)
                };
                set(Resource::Cpu, cpu_secs)?;
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    set(Resource::As, memory)?;
                    set(Resource::Nproc, processes)?;
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                {
                    let _ = (memory, processes);
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        let pgid = child.id();
        Ok((child, Group { pgid }))
    }

    impl Group {
        /// SIGKILL to every process of the group.
        pub fn kill(&self) {
            if let Some(pid) = i32::try_from(self.pgid).ok().and_then(Pid::from_raw) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }
    }
}

// ----------------------------------------------------------------------------------- Windows

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 Job Object FFI; every call is confined to this module.
mod os {
    use std::io;
    use std::mem;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    use super::ChildLimits;

    /// The job holding the child's whole tree. Closing it kills what is left in it.
    pub struct Group {
        job: usize,
    }

    /// `PerProcessUserTimeLimit` counts 100 ns ticks.
    const TICKS_PER_SECOND: i64 = 10_000_000;
    /// Exit code of processes killed by asc.
    const KILL_EXIT_CODE: u32 = 1;

    fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    fn new_job(limits: &ChildLimits) -> io::Result<Group> {
        // SAFETY: plain Win32 calls with null security attributes and a null name; the info
        // struct is zeroed and its size is passed.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(last_error());
            }
            let group = Group { job: job as usize };
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_JOB_MEMORY
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_TIME;
            let ticks = i64::try_from(limits.cpu.as_secs())
                .unwrap_or(i64::MAX / TICKS_PER_SECOND)
                .saturating_mul(TICKS_PER_SECOND);
            info.BasicLimitInformation.PerProcessUserTimeLimit = ticks.max(TICKS_PER_SECOND);
            info.BasicLimitInformation.ActiveProcessLimit = limits.processes;
            let process = usize::try_from(limits.memory).unwrap_or(usize::MAX);
            info.ProcessMemoryLimit = process;
            info.JobMemoryLimit = process.saturating_mul(2);
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(last_error());
            }
            Ok(group)
        }
    }

    /// Resumes every thread of a process created suspended.
    fn resume_process(pid: u32) -> io::Result<()> {
        // SAFETY: the snapshot and thread handles are opened and closed here; the entry struct
        // is zeroed with its size set as the API requires.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return Err(last_error());
            }
            let mut entry: THREADENTRY32 = mem::zeroed();
            entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
            let mut resumed = 0u32;
            let mut more = Thread32First(snap, &mut entry);
            while more != 0 {
                if entry.th32OwnerProcessID == pid {
                    let thread: HANDLE = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        if ResumeThread(thread) != u32::MAX {
                            resumed += 1;
                        }
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snap, &mut entry);
            }
            CloseHandle(snap);
            if resumed == 0 {
                return Err(io::Error::other("the child has no thread to resume"));
            }
            Ok(())
        }
    }

    /// Starts the child suspended, puts it in the job, then lets it run, so that nothing it
    /// spawns can escape the job.
    pub fn spawn(mut cmd: Command, limits: &ChildLimits) -> io::Result<(Child, Group)> {
        let group = new_job(limits)?;
        cmd.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = cmd.spawn()?;
        // SAFETY: both handles are live: the job is owned by `group`, the process by `child`.
        let assigned = unsafe {
            AssignProcessToJobObject(group.job as HANDLE, child.as_raw_handle() as HANDLE)
        };
        let started = if assigned == 0 {
            Err(last_error())
        } else {
            resume_process(child.id())
        };
        if let Err(e) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok((child, group))
    }

    impl Group {
        /// Terminates every process in the job.
        pub fn kill(&self) {
            // SAFETY: the job handle is open until `drop`.
            unsafe {
                TerminateJobObject(self.job as HANDLE, KILL_EXIT_CODE);
            }
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            // SAFETY: the handle was created by `new_job` and is closed once.
            unsafe {
                CloseHandle(self.job as HANDLE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(deadline_ms: u64, idle_ms: u64) -> ChildLimits {
        ChildLimits {
            memory: 4 * 1024 * 1024 * 1024,
            cpu: Duration::from_secs(60),
            processes: 64,
            deadline: Duration::from_millis(deadline_ms),
            idle: Duration::from_millis(idle_ms),
        }
    }

    /// `RLIMIT_NPROC` is per UID, so a test that forks in bulk or starts many threads shrinks the
    /// room of every child spawned at the same moment. Tests that spawn share this gate; the one
    /// that does that on purpose holds it exclusively.
    static SPAWN_GATE: std::sync::RwLock<()> = std::sync::RwLock::new(());

    fn spawn_gate() -> std::sync::RwLockReadGuard<'static, ()> {
        SPAWN_GATE.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// A shell running `script` (cmd on Windows, sh elsewhere).
    fn shell(script: &str) -> Command {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut cmd = Command::new("cmd"); // lint:allow-raw-git
            cmd.arg("/c").raw_arg(script);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = Command::new("sh"); // lint:allow-raw-git
            cmd.arg("-c").arg(script);
            cmd
        }
    }

    /// A shell that starts a grandchild and then waits for it.
    fn tree_command(marker: &Path) -> Command {
        if cfg!(windows) {
            shell(&format!(
                "ping -n 30 127.0.0.1 > nul & echo grandchild > {}",
                marker.display()
            ))
        } else {
            shell(&format!(
                "sleep 30 & echo grandchild > '{}'; wait",
                marker.display()
            ))
        }
    }

    #[test]
    fn the_deadline_kills_the_child() {
        let _gate = spawn_gate();
        let dir = tempfile::tempdir().unwrap();
        let cmd = tree_command(&dir.path().join("m"));
        let child = spawn_supervised(cmd, &limits(400, 60_000), true).unwrap();
        let started = Instant::now();
        let exit = child.wait().unwrap();
        assert_eq!(exit.killed, Some(KillReason::Deadline));
        assert!(!exit.status.success());
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn silence_kills_the_child_but_activity_keeps_it_alive() {
        let _gate = spawn_gate();
        let dir = tempfile::tempdir().unwrap();
        let cmd = tree_command(&dir.path().join("m"));
        let child = spawn_supervised(cmd, &limits(60_000, 500), true).unwrap();
        let act = child.activity();
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(200));
            act.touch();
        }
        assert!(
            child.shared.reason.load(Ordering::SeqCst) == 0,
            "kept alive by touch"
        );
        let exit = child.wait().unwrap();
        assert_eq!(exit.killed, Some(KillReason::Idle));
    }

    #[test]
    fn dropping_a_child_kills_it_and_a_requested_kill_is_reported() {
        let _gate = spawn_gate();
        let dir = tempfile::tempdir().unwrap();
        let child = spawn_supervised(
            tree_command(&dir.path().join("m")),
            &limits(60_000, 60_000),
            true,
        )
        .unwrap();
        child.kill();
        let exit = child.wait().unwrap();
        assert_eq!(exit.killed, Some(KillReason::Requested));
        let dropped = spawn_supervised(
            tree_command(&dir.path().join("m")),
            &limits(60_000, 60_000),
            true,
        )
        .unwrap();
        let started = Instant::now();
        drop(dropped);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn the_whole_tree_dies_with_the_child() {
        let _gate = spawn_gate();
        // The grandchild writes the marker only after its sleep; killing the tree early means
        // the marker never appears.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");
        let cmd = if cfg!(windows) {
            shell(&format!(
                "start /b cmd /c \"ping -n 4 127.0.0.1 > nul & echo x > {}\" & ping -n 30 127.0.0.1 > nul",
                marker.display()
            ))
        } else {
            shell(&format!(
                "(sleep 2; echo x > '{}') & sleep 30",
                marker.display()
            ))
        };
        let child = spawn_supervised(cmd, &limits(500, 60_000), true).unwrap();
        let exit = child.wait().unwrap();
        assert_eq!(exit.killed, Some(KillReason::Deadline));
        std::thread::sleep(Duration::from_secs(6));
        assert!(!marker.exists(), "a grandchild outlived the kill");
    }

    #[test]
    fn captured_output_is_bounded_and_complete() {
        let _gate = spawn_gate();
        let cmd = shell("echo hello");
        let out = run_captured(cmd, b"", &limits(30_000, 30_000)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[test]
    fn the_memory_limit_stops_a_child_that_outgrows_it() {
        let _gate = spawn_gate();
        // `git hash-object --stdin` holds the whole blob in memory.
        let git = RealGit::resolve(None, None).expect("real git");
        let hash = || {
            let mut cmd = git.command().expect("command");
            cmd.args(["hash-object", "--stdin", "--no-filters"]);
            cmd
        };
        let big = vec![7u8; 160 * 1024 * 1024];
        let mut tight = limits(120_000, 120_000);
        tight.memory = 96 * 1024 * 1024;
        let out = run_captured(hash(), &big, &tight).unwrap();
        assert!(
            !out.status.success(),
            "a 160 MiB blob must not fit in 96 MiB"
        );
        let roomy = limits(120_000, 120_000);
        let out = run_captured(hash(), &big, &roomy).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim().len(), 40);
    }

    #[test]
    fn ref_names_follow_check_ref_format() {
        for ok in [
            "refs/heads/master",
            "refs/tags/v1.0",
            "refs/heads/a/b-c_d",
            "refs/heads/@x",
        ] {
            assert!(valid_ref_name(ok), "{ok}");
        }
        for bad in [
            "",
            "/refs/heads/x",
            "refs/heads/x/",
            "refs/heads/x.",
            "refs//x",
            "refs/heads/a..b",
            "refs/heads/a b",
            "refs/heads/a\nb",
            "refs/heads/a\0b",
            "refs/heads/a~b",
            "refs/heads/a^b",
            "refs/heads/a:b",
            "refs/heads/a?b",
            "refs/heads/a*b",
            "refs/heads/a[b",
            "refs/heads/a\\b",
            "refs/heads/.hidden",
            "refs/heads/x.lock",
            "refs/heads/x@{1}",
            "@",
        ] {
            assert!(!valid_ref_name(bad), "{bad:?}");
        }
        assert!(!valid_ref_name(&format!(
            "refs/heads/{}",
            "a".repeat(MAX_REF_NAME_BYTES)
        )));
    }

    #[test]
    fn migration_never_relaxes_a_dangerous_class() {
        for id in MIGRATION_RELAXED {
            assert!(
                !id.starts_with("gitmodules")
                    && !id.to_lowercase().contains("dotgit")
                    && !id.starts_with("nul"),
                "{id}"
            );
        }
        for dangerous in [
            "hasDotgit",
            "gitmodulesSymlink",
            "gitmodulesName",
            "gitmodulesPath",
            "gitmodulesUrl",
            "gitmodulesUpdate",
            "gitmodulesParse",
            "gitmodulesMissing",
            "gitmodulesBlob",
            "gitmodulesLarge",
            "nulInHeader",
            "nulInCommit",
        ] {
            assert!(!MIGRATION_RELAXED.contains(&dangerous), "{dangerous}");
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn the_nproc_cap_is_relative_and_never_below_one_more_than_now() {
        assert_eq!(os::relative_nproc(500, 64), 564);
        assert_eq!(os::relative_nproc(500, 0), 501);
        assert_eq!(os::relative_nproc(u64::MAX, 64), u64::MAX);
    }

    /// `RLIMIT_NPROC` counts every task of the UID, so a server with more threads than
    /// `limits.processes` must still be able to start a child, and that child must still be
    /// capped at `limits.processes` more tasks.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_starts_while_the_parent_holds_more_threads_than_the_cap_and_cannot_fork_past_it() {
        use std::sync::mpsc;

        let _gate = SPAWN_GATE.write().unwrap_or_else(PoisonError::into_inner);

        let (release, parked) = mpsc::channel::<()>();
        let parked = Arc::new(Mutex::new(parked));
        let threads: Vec<_> = (0..100)
            .map(|_| {
                let parked = Arc::clone(&parked);
                std::thread::spawn(move || {
                    let _ = parked.lock().unwrap_or_else(PoisonError::into_inner).recv();
                })
            })
            .collect();

        let mut lim = limits(60_000, 60_000);
        lim.processes = 40;

        // The child's soft limit is relative: more than the 100 parked threads plus the cap.
        let probe = run_captured(
            shell("awk '/Max processes/ {print $3}' /proc/self/limits"),
            b"",
            &lim,
        )
        .unwrap_or_else(|_| panic!("spawn with 100+ threads"));
        let nproc: u64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap();
        assert!(nproc >= 100 + 40, "RLIMIT_NPROC {nproc} is not relative");

        // Each successful background fork appends a line; a failed fork ends the shell.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("forks");
        let bomb = shell(&format!(
            "i=0; while [ $i -lt 400 ]; do sleep 30 & echo x >> '{}'; i=$((i+1)); done",
            log.display()
        ));
        let child = spawn_supervised(bomb, &lim, true).unwrap();
        let _ = child.wait().unwrap();
        let forks = fs::read_to_string(&log).unwrap_or_default().lines().count();

        drop(release);
        for t in threads {
            let _ = t.join();
        }
        // The cap is 40 tasks past the count at spawn; threads of other tests that exit meanwhile
        // widen it a little, an unlimited child would reach 400.
        assert!(forks >= 1, "the child forked nothing");
        assert!(
            forks < 200,
            "the child forked {forks} times past a cap of 40"
        );
    }
}
