//! The `asc-serve.toml` loader (SPEC §8.1, §16.3).
//!
//! The file is operator input but still parsed defensively: it is size-capped, unknown keys are
//! rejected, every count and size is bounded, and secrets are only ever named by file path.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use asc_core::transport_policy::{Policy, Transport, classify, require_network};
use asc_rpc::server::RepoName;
use serde::{Deserialize, Deserializer};

/// Largest accepted config file.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Most `[[composition_grants]]` entries.
pub const MAX_GRANTS: usize = 10_000;

/// Why a config was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is larger than {MAX_CONFIG_BYTES} bytes")]
    TooLarge(PathBuf),
    #[error("{0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

/// A byte count written as `4GiB`, `512MB` or a plain integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const KIB: u64 = 1024;
    pub const MIB: u64 = 1024 * Self::KIB;
    pub const GIB: u64 = 1024 * Self::MIB;

    /// Parses a size with an optional unit (`B`, `KiB`..`TiB`, `KB`..`TB`); no unit means bytes.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: u64 = num.parse().map_err(|_| format!("bad size {s:?}"))?;
        let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "kib" => 1 << 10,
            "mib" => 1 << 20,
            "gib" => 1 << 30,
            "tib" => 1 << 40,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "tb" => 1_000_000_000_000,
            _ => return Err(format!("bad size unit in {s:?}")),
        };
        n.checked_mul(mult)
            .map(Self)
            .ok_or_else(|| format!("size {s:?} overflows"))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(Self(n)),
            Raw::Text(t) => Self::parse(&t).map_err(serde::de::Error::custom),
        }
    }
}

/// A duration written as `15m`, `60s`, `1h`, `2d` or a plain integer of seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Secs(pub u64);

impl Secs {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
        let mult: u64 = match unit.trim() {
            "" | "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => return Err(format!("bad duration unit in {s:?}")),
        };
        n.checked_mul(mult)
            .map(Self)
            .ok_or_else(|| format!("duration {s:?} overflows"))
    }
}

impl<'de> Deserialize<'de> for Secs {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(Self(n)),
            Raw::Text(t) => Self::parse(&t).map_err(serde::de::Error::custom),
        }
    }
}

/// What this process does (SPEC §7, §23.10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full server over SQL and the bucket.
    #[default]
    Primary,
    /// Tier RPCs only, backed by an upstream; no SQL.
    Cache,
    /// Code-intelligence workers only.
    Indexer,
}

/// A `tls = { cert, key }` pair; both are file paths.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    pub memory: ByteSize,
    pub disk_dir: Option<PathBuf>,
    pub disk: ByteSize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory: ByteSize(ByteSize::GIB),
            disk_dir: None,
            disk: ByteSize(0),
        }
    }
}

/// `[admin]`: the separate listener for `/metrics` and the detailed `/readyz` (SPEC §8.1).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AdminConfig {
    pub listen: SocketAddr,
    /// File holding the bearer token every admin route requires. Mandatory when `listen` is not
    /// a loopback address.
    pub token_file: Option<PathBuf>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 9464)),
            token_file: None,
        }
    }
}

/// How `asc serve` writes its log lines (`[log] format`, or `ASC_LOG_FORMAT`, which wins).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines (the default).
    #[default]
    Text,
    /// One JSON object per line, for log shippers.
    Json,
}

/// `[log]`: the log format. The level filter is the `RUST_LOG` environment variable (default `info`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    pub format: LogFormat,
}

/// `[forgejo]`: the optional Forgejo adapter (`provider = "forgejo"`, hooks, mirror webhook).
/// Built only with the `forgejo` feature.
#[cfg(feature = "forgejo")]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgejoConfig {
    pub url: String,
    pub internal_token_file: Option<PathBuf>,
    pub webhook_secret_file: Option<PathBuf>,
    pub repo_root: Option<PathBuf>,
    /// Keep the host's bare git directories under `repo_root` current: after every ref update a
    /// host writer made, the `bridge_promote` outbox effect exports the new tips into
    /// `<repo_root>/<owner>/<name>.git` and sets those refs to asc's values. Needs `repo_root`,
    /// a writable mount of it and `[git] enabled`. Set it exactly when the host's bridge mode is
    /// `eager`; with `lazy` or `off` the host builds or refuses the directory itself.
    #[serde(default)]
    pub bridge_promote: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TurboConfig {
    pub trusted_tokens_file: Option<PathBuf>,
    pub pr_retention_days: u32,
    pub trusted_retention_days: u32,
}

impl Default for TurboConfig {
    fn default() -> Self {
        Self {
            trusted_tokens_file: None,
            pr_retention_days: 14,
            trusted_retention_days: 60,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    pub enabled: bool,
    pub write: bool,
    pub allowed_origins: Vec<String>,
    /// Host names (optionally `:port`) the endpoint answers to besides `public_url` and the
    /// loopback names of the listen address. Any other `Host` is refused with 421 (DNS rebinding).
    pub allowed_hosts: Vec<String>,
    /// The read toolsets `tools/list` offers: any of `core`, `history`, `stack`, `projects`,
    /// `search`, or `all`. The default is every read toolset; write tools follow `write`.
    pub toolsets: Vec<String>,
    /// When set, only these tools are offered (and callable), within `toolsets`. Forge tools are
    /// named here too.
    pub tools: Option<Vec<String>>,
    /// `[mcp.forge]`: the optional forge tools. Off unless an adapter is named.
    pub forge: crate::mcp::forge::ForgeConfig,
    pub hmac_keys: Vec<PathBuf>,
    pub max_tokens: u32,
    pub max_inflight_per_principal: u32,
    /// `[mcp.oauth]` (M33): absent means no OAuth.
    pub oauth: Option<crate::mcp::oauth::McpOauthConfig>,
}

impl McpConfig {
    /// Checks `allowed_hosts`, `toolsets`, `tools` and `[mcp.forge]`.
    fn validate_exposure(&self) -> Result<(), ConfigError> {
        for h in &self.allowed_hosts {
            let t = h.trim();
            let bad = t.is_empty()
                || t.len() > 255
                || t.contains(['/', '?', '#', '@', '*', ' ', '\\'])
                || t.contains("://");
            if bad {
                return invalid(format!(
                    "mcp.allowed_hosts: {h:?} is not a host name (no scheme, path or wildcard)"
                ));
            }
        }
        for t in &self.toolsets {
            let t = t.trim();
            if t != "all" && asc_mcp::catalogue::Toolset::parse(t).is_none() {
                return invalid(format!(
                    "mcp.toolsets: unknown toolset {t:?} (core, history, stack, projects, search, all)"
                ));
            }
            if t == "write" {
                return invalid("mcp.toolsets: write tools follow `mcp.write`, not a toolset");
            }
        }
        for t in self.tools.iter().flatten() {
            let known = asc_mcp::catalogue::find(t, asc_mcp::schema::Surface::Http).is_some()
                || crate::mcp::forge::FORGE_TOOLS.contains(&t.as_str());
            if !known {
                return invalid(format!("mcp.tools: unknown tool {t:?}"));
            }
        }
        self.forge
            .validate()
            .map_err(|e| ConfigError::Invalid(format!("mcp.forge: {e}")))?;
        if self.forge.enabled() && !self.enabled {
            return invalid("mcp.forge needs mcp.enabled");
        }
        Ok(())
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            write: false,
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
            toolsets: ["core", "history", "stack", "projects", "search"]
                .map(str::to_owned)
                .to_vec(),
            tools: None,
            forge: crate::mcp::forge::ForgeConfig::default(),
            hmac_keys: Vec::new(),
            max_tokens: 6000,
            max_inflight_per_principal: 8,
            oauth: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbedConfig {
    pub model: String,
    pub device: String,
    pub provider: String,
    /// The directory of the `asc-embed` helper, `model/` and `calibration.bin` (the layout
    /// `asc codeintel enable-embed` installs). Without it `Semantic` answers
    /// `FAILED_PRECONDITION` and says how to configure it.
    pub dir: Option<PathBuf>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model: "jina-embeddings-v2-base-code".into(),
            device: "cpu".into(),
            provider: "local".into(),
            dir: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CodeIntelConfig {
    pub enabled: bool,
    pub memory: ByteSize,
    pub workers: u32,
    pub embed: EmbedConfig,
}

impl Default for CodeIntelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            memory: ByteSize(2 * ByteSize::GIB),
            workers: 4,
            embed: EmbedConfig::default(),
        }
    }
}

/// `[bucket]`: per-replica bucket call bounds (SPEC §16.3).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BucketConfig {
    pub max_concurrency: u32,
    pub timeout_secs: u32,
}

impl Default for BucketConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 256,
            timeout_secs: 30,
        }
    }
}

/// How hard fsck is on pushed objects (SPEC §25.8 item 5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitFsck {
    /// Git's defaults for every check.
    #[default]
    Strict,
    /// Old histories from other hosts: the harmless legacy classes are ignored.
    Migration,
}

/// `[git]` (SPEC §25.9): honoured only in a build with the Cargo feature `git-compat`, but
/// parsed in every build, so one config file works with either.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitConfig {
    /// The runtime switch; `false` serves the disabled stub of U78.
    pub enabled: bool,
    /// The git executable; empty means real git resolution (SPEC §11.4).
    pub git_path: String,
    /// Where the git views live; empty means `<cache disk_dir>/gitview`.
    pub view_dir: String,
    pub fsck: GitFsck,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            git_path: String::new(),
            view_dir: String::new(),
            fsck: GitFsck::Strict,
        }
    }
}

impl GitConfig {
    /// `git_path` when set.
    pub fn git_path(&self) -> Option<PathBuf> {
        (!self.git_path.is_empty()).then(|| PathBuf::from(&self.git_path))
    }

    /// `view_dir` when set, else `<cache disk_dir>/gitview`.
    pub fn view_dir(&self, cache: &CacheConfig) -> Option<PathBuf> {
        if self.view_dir.is_empty() {
            cache.disk_dir.as_ref().map(|d| d.join("gitview"))
        } else {
            Some(PathBuf::from(&self.view_dir))
        }
    }
}

/// `[limits.git]` (SPEC §25.5). Defaults are the SPEC values.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitLimitsConfig {
    pub max_push_bytes: ByteSize,
    pub max_push_objects: u64,
    pub max_object_bytes: ByteSize,
    pub request_timeout: Secs,
    pub idle_timeout: Secs,
    pub child_memory: ByteSize,
    pub child_processes: u32,
    pub pack_threads: u32,
    pub min_free_disk: ByteSize,
    pub fetch_body_bytes: ByteSize,
    pub fetch_per_principal: u32,
    pub fetch_per_replica: u32,
    pub push_per_principal: u32,
    pub push_per_replica: u32,
    pub view_builds_per_replica: u32,
}

impl Default for GitLimitsConfig {
    fn default() -> Self {
        Self {
            max_push_bytes: ByteSize(2 * ByteSize::GIB),
            max_push_objects: 2_000_000,
            max_object_bytes: ByteSize(512 * ByteSize::MIB),
            request_timeout: Secs(15 * 60),
            idle_timeout: Secs(60),
            child_memory: ByteSize(2 * ByteSize::GIB),
            child_processes: 64,
            pack_threads: 2,
            min_free_disk: ByteSize(10 * ByteSize::GIB),
            fetch_body_bytes: ByteSize(16 * ByteSize::MIB),
            fetch_per_principal: 4,
            fetch_per_replica: 16,
            push_per_principal: 2,
            push_per_replica: 8,
            view_builds_per_replica: 2,
        }
    }
}

impl GitLimitsConfig {
    /// The values `asc-git` enforces; `asc-git` applies its own ceilings to them.
    #[cfg(feature = "git-compat")]
    pub fn to_limits(&self) -> asc_git::compat::GitLimits {
        use std::time::Duration;
        asc_git::compat::GitLimits {
            max_push_bytes: self.max_push_bytes.0,
            max_push_objects: self.max_push_objects,
            max_object_bytes: self.max_object_bytes.0,
            request_timeout: Duration::from_secs(self.request_timeout.0),
            idle_timeout: Duration::from_secs(self.idle_timeout.0),
            child_memory: self.child_memory.0,
            child_processes: self.child_processes,
            pack_threads: self.pack_threads,
            min_free_disk: self.min_free_disk.0,
            fetch_body_bytes: self.fetch_body_bytes.0,
            fetch_per_principal: self.fetch_per_principal,
            fetch_per_replica: self.fetch_per_replica,
            push_per_principal: self.push_per_principal,
            push_per_replica: self.push_per_replica,
            view_builds_per_replica: self.view_builds_per_replica,
        }
    }

    /// Positive values; with the feature on, `asc-git` applies its ceilings and ordering too.
    fn validate(&self) -> Result<(), ConfigError> {
        let positive = [
            ("max_push_bytes", self.max_push_bytes.0),
            ("max_push_objects", self.max_push_objects),
            ("max_object_bytes", self.max_object_bytes.0),
            ("request_timeout", self.request_timeout.0),
            ("idle_timeout", self.idle_timeout.0),
            ("child_memory", self.child_memory.0),
            ("child_processes", self.child_processes.into()),
            ("pack_threads", self.pack_threads.into()),
            ("fetch_body_bytes", self.fetch_body_bytes.0),
            ("fetch_per_principal", self.fetch_per_principal.into()),
            ("fetch_per_replica", self.fetch_per_replica.into()),
            ("push_per_principal", self.push_per_principal.into()),
            ("push_per_replica", self.push_per_replica.into()),
            (
                "view_builds_per_replica",
                self.view_builds_per_replica.into(),
            ),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return invalid(format!("limits.git.{name} must be greater than zero"));
        }
        #[cfg(feature = "git-compat")]
        self.to_limits()
            .validate()
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(())
    }
}

/// The default host key file of `[ssh] host_key_files` (SPEC §25.11.10).
pub const DEFAULT_SSH_HOST_KEY: &str = "/etc/asc/ssh_host_ed25519_key";
/// Most host key files `[ssh] host_key_files` may list.
pub const MAX_SSH_HOST_KEYS: usize = 8;

/// `[ssh]` (SPEC §25.11.10): honoured only in a build with the Cargo feature `git-ssh`, but
/// parsed in every build, so one config file works with either.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SshConfig {
    /// The runtime switch; needs `[git] enabled = true`.
    pub enabled: bool,
    pub listen: SocketAddr,
    /// The only accepted SSH user name; empty accepts any.
    pub user: String,
    /// OpenSSH-format private host keys. A missing first file is generated once.
    pub host_key_files: Vec<PathBuf>,
    /// RSA user keys under this many bits are refused.
    pub min_rsa_bits: u32,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: SocketAddr::from(([0, 0, 0, 0], 2222)),
            user: "git".into(),
            host_key_files: vec![PathBuf::from(DEFAULT_SSH_HOST_KEY)],
            min_rsa_bits: 2048,
        }
    }
}

impl SshConfig {
    /// Smallest `min_rsa_bits` accepted: weaker keys are never allowed in.
    pub const MIN_RSA_FLOOR: u32 = 1024;
    /// Largest RSA key accepted (the same ceiling as the key store).
    pub const MAX_RSA_BITS: u32 = 16_384;

    fn validate(&self) -> Result<(), ConfigError> {
        if self.user.len() > 64
            || self
                .user
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return invalid("ssh.user must be at most 64 characters with no whitespace");
        }
        if self.host_key_files.is_empty() || self.host_key_files.len() > MAX_SSH_HOST_KEYS {
            return invalid(format!(
                "ssh.host_key_files must list 1 to {MAX_SSH_HOST_KEYS} files"
            ));
        }
        if self.host_key_files.iter().any(|p| p.as_os_str().is_empty()) {
            return invalid("ssh.host_key_files holds an empty path");
        }
        if !(Self::MIN_RSA_FLOOR..=Self::MAX_RSA_BITS).contains(&self.min_rsa_bits) {
            return invalid(format!(
                "ssh.min_rsa_bits must be between {} and {}",
                Self::MIN_RSA_FLOOR,
                Self::MAX_RSA_BITS
            ));
        }
        Ok(())
    }
}

/// `[limits.ssh]` (SPEC §25.11.7). Defaults are the SPEC values.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SshLimitsConfig {
    /// Connections per replica.
    pub max_connections: u32,
    /// Authenticated connections per principal.
    pub max_connections_per_principal: u32,
    /// Connections that have not authenticated yet, per replica and per peer.
    pub max_unauthenticated: u32,
    pub max_unauthenticated_per_peer: u32,
    /// New connections per second per peer, and the burst.
    pub new_connections_per_sec: u32,
    pub new_connections_burst: u32,
    pub auth_timeout: Secs,
    /// Rejected authentication attempts before the server disconnects.
    pub max_auth_tries: u32,
    /// Every rejected attempt takes this long to answer, so that its timing says nothing.
    pub reject_delay_ms: u64,
    pub keepalive: Secs,
    /// Unanswered keep-alives before the connection closes.
    pub keepalive_max: u32,
    /// Session channels per connection, and commands that may run at once.
    pub max_channels: u32,
    pub max_running_commands: u32,
    /// What a channel may buffer: the window and the largest packet.
    pub window_size: ByteSize,
    pub packet_size: ByteSize,
    /// A command's wall-clock limit, and how long one may sit without moving a byte.
    pub request_timeout: Secs,
    pub idle_timeout: Secs,
}

impl Default for SshLimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_connections_per_principal: 16,
            max_unauthenticated: 64,
            max_unauthenticated_per_peer: 8,
            new_connections_per_sec: 10,
            new_connections_burst: 20,
            auth_timeout: Secs(30),
            max_auth_tries: 6,
            reject_delay_ms: 500,
            keepalive: Secs(30),
            keepalive_max: 3,
            max_channels: 4,
            max_running_commands: 2,
            window_size: ByteSize(2 * ByteSize::MIB),
            packet_size: ByteSize(32 * ByteSize::KIB),
            request_timeout: Secs(15 * 60),
            idle_timeout: Secs(60),
        }
    }
}

impl SshLimitsConfig {
    /// Positive values within their ceilings, and the tiers in order.
    fn validate(&self) -> Result<(), ConfigError> {
        let bounded = [
            (
                "max_connections",
                u64::from(self.max_connections),
                1_000_000,
            ),
            (
                "max_connections_per_principal",
                u64::from(self.max_connections_per_principal),
                10_000,
            ),
            (
                "max_unauthenticated",
                u64::from(self.max_unauthenticated),
                100_000,
            ),
            (
                "max_unauthenticated_per_peer",
                u64::from(self.max_unauthenticated_per_peer),
                1_000,
            ),
            (
                "new_connections_per_sec",
                u64::from(self.new_connections_per_sec),
                10_000,
            ),
            (
                "new_connections_burst",
                u64::from(self.new_connections_burst),
                100_000,
            ),
            ("auth_timeout", self.auth_timeout.0, 300),
            ("max_auth_tries", u64::from(self.max_auth_tries), 20),
            ("keepalive", self.keepalive.0, 3600),
            ("keepalive_max", u64::from(self.keepalive_max), 20),
            ("max_channels", u64::from(self.max_channels), 64),
            (
                "max_running_commands",
                u64::from(self.max_running_commands),
                64,
            ),
            ("window_size", self.window_size.0, 64 * ByteSize::MIB),
            ("packet_size", self.packet_size.0, 65_535),
            ("request_timeout", self.request_timeout.0, 24 * 3600),
            ("idle_timeout", self.idle_timeout.0, 24 * 3600),
        ];
        for (name, value, ceiling) in bounded {
            if value == 0 || value > ceiling {
                return invalid(format!("limits.ssh.{name} must be between 1 and {ceiling}"));
            }
        }
        if self.reject_delay_ms > 5000 {
            return invalid("limits.ssh.reject_delay_ms must be at most 5000");
        }
        if self.packet_size.0 < 1024 || self.window_size.0 < self.packet_size.0 {
            return invalid("limits.ssh.packet_size must be at least 1 KiB and fit the window");
        }
        if self.max_unauthenticated_per_peer > self.max_unauthenticated
            || self.max_unauthenticated > self.max_connections
        {
            return invalid(
                "limits.ssh: max_unauthenticated_per_peer <= max_unauthenticated <= max_connections",
            );
        }
        if self.max_running_commands > self.max_channels {
            return invalid("limits.ssh.max_running_commands must not exceed max_channels");
        }
        if self.idle_timeout.0 > self.request_timeout.0 {
            return invalid("limits.ssh.idle_timeout must not exceed request_timeout");
        }
        Ok(())
    }
}

/// `[limits]`: admission control (SPEC §16.3). Defaults are the SPEC values.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_inflight: u32,
    pub max_inflight_per_token: u32,
    /// Bytes of whole-file buffers all requests may hold at once (the `TextBudget`).
    pub text_memory: ByteSize,
    pub cheap_rps: u32,
    pub cheap_burst: u32,
    pub objects_bytes_per_sec: ByteSize,
    pub expensive_per_principal: u32,
    pub expensive_per_replica: u32,
    pub update_refs_rps_per_repo: u32,
    pub codeintel_rps: u32,
    pub codeintel_burst: u32,
    pub semantic_rps: u32,
    pub semantic_concurrency: u32,
    pub service_rps: u32,
    pub service_burst: u32,
    /// `WatchRefs` streams one replica serves at once (SPEC 21.3.8).
    pub watch_streams: u32,
    /// `WatchRefs` streams one principal may hold at once. Anonymous callers share one principal.
    pub watch_streams_per_principal: u32,
    /// Of `watch_streams`, how many anonymous callers may hold together; the rest are for
    /// verified callers.
    pub watch_streams_anonymous: u32,
    /// `WatchRefs` streams one anonymous peer address may hold (a trusted proxy's
    /// `X-Forwarded-For` client counts as the peer, as in admission).
    pub watch_streams_per_peer: u32,
    /// The server ends a `WatchRefs` stream after this long; the client resumes.
    pub watch_max_lifetime: Secs,
    /// The server ends a `WatchRefs` stream that delivered no event for this long.
    pub watch_idle_timeout: Secs,
    /// CIDRs (or bare addresses) of reverse proxies in front of this server. Only a connection
    /// from one of them may name the real client in `X-Forwarded-For`; any other peer's header is
    /// ignored. Empty by default.
    pub trusted_proxies: Vec<String>,
    /// `[limits.git]` (SPEC §25.5).
    pub git: GitLimitsConfig,
    /// `[limits.ssh]` (SPEC §25.11.7); honoured only with the feature `git-ssh`.
    pub ssh: SshLimitsConfig,
    /// `[limits.h3]` (U89b): the QUIC listener; honoured only with the feature `http3`.
    pub h3: crate::h3_listener::H3LimitsConfig,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_inflight: 1024,
            max_inflight_per_token: 64,
            text_memory: ByteSize(2 * ByteSize::GIB),
            cheap_rps: 200,
            cheap_burst: 400,
            objects_bytes_per_sec: ByteSize(400 * ByteSize::MIB),
            expensive_per_principal: 20,
            expensive_per_replica: 64,
            update_refs_rps_per_repo: 20,
            codeintel_rps: 50,
            codeintel_burst: 100,
            semantic_rps: 10,
            semantic_concurrency: 32,
            service_rps: 1000,
            service_burst: 2000,
            watch_streams: 1024,
            watch_streams_per_principal: 8,
            watch_streams_anonymous: 256,
            watch_streams_per_peer: 4,
            watch_max_lifetime: Secs(60 * 60),
            watch_idle_timeout: Secs(10 * 60),
            trusted_proxies: Vec::new(),
            git: GitLimitsConfig::default(),
            ssh: SshLimitsConfig::default(),
            h3: crate::h3_listener::H3LimitsConfig::default(),
        }
    }
}

impl Limits {
    fn validate(&self) -> Result<(), ConfigError> {
        let counts = [
            ("max_inflight", self.max_inflight),
            ("max_inflight_per_token", self.max_inflight_per_token),
            ("cheap_rps", self.cheap_rps),
            ("cheap_burst", self.cheap_burst),
            ("expensive_per_principal", self.expensive_per_principal),
            ("expensive_per_replica", self.expensive_per_replica),
            ("update_refs_rps_per_repo", self.update_refs_rps_per_repo),
            ("codeintel_rps", self.codeintel_rps),
            ("codeintel_burst", self.codeintel_burst),
            ("semantic_rps", self.semantic_rps),
            ("semantic_concurrency", self.semantic_concurrency),
            ("service_rps", self.service_rps),
            ("service_burst", self.service_burst),
            ("watch_streams", self.watch_streams),
            (
                "watch_streams_per_principal",
                self.watch_streams_per_principal,
            ),
            ("watch_streams_anonymous", self.watch_streams_anonymous),
            ("watch_streams_per_peer", self.watch_streams_per_peer),
        ];
        if let Some((name, _)) = counts.iter().find(|(_, v)| *v == 0) {
            return invalid(format!("limits.{name} must be greater than zero"));
        }
        if self.watch_streams_anonymous > self.watch_streams {
            return invalid("limits.watch_streams_anonymous must not exceed watch_streams");
        }
        if self.watch_idle_timeout.0 == 0 || self.watch_idle_timeout.0 > self.watch_max_lifetime.0 {
            return invalid(
                "limits.watch_idle_timeout must be positive and not exceed watch_max_lifetime",
            );
        }
        if let Some(bad) = self
            .trusted_proxies
            .iter()
            .find(|p| p.parse::<crate::limits::Cidr>().is_err())
        {
            return invalid(format!("limits.trusted_proxies: `{bad}` is not a CIDR"));
        }
        if self.text_memory.0 == 0 || self.objects_bytes_per_sec.0 == 0 {
            return invalid("limits.text_memory and limits.objects_bytes_per_sec must be positive");
        }
        self.ssh.validate()?;
        self.h3.validate()?;
        self.git.validate()
    }
}

/// Which `Authorizer` a server runs (`[auth] provider`, SPEC §15.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthProvider {
    /// Delegation to Forgejo (`"forgejo"`); needs `[forgejo]` and the `forgejo` feature.
    Forgejo,
    /// The standalone token store (M30); needs `auth.token_key_file`.
    TokenStore,
}

impl<'de> Deserialize<'de> for AuthProvider {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        match name.as_str() {
            "forgejo" => Ok(Self::Forgejo),
            "token_store" => Ok(Self::TokenStore),
            // The name before the Forgejo adapter became optional.
            "forge" => {
                tracing::warn!(
                    "auth.provider = \"forge\" is deprecated; write auth.provider = \"forgejo\""
                );
                Ok(Self::Forgejo)
            }
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["forgejo", "token_store"],
            )),
        }
    }
}

/// `[auth]`: the one authorizer for RPC, HTTP routes and MCP.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// `forgejo` or `token_store`; `forge` is a deprecated spelling of `forgejo`. Always explicit:
    /// a `[forgejo]` section no longer selects it. Absent, every credential is refused.
    pub provider: Option<AuthProvider>,
    /// The file holding the token store's HMAC key (hex, 32 bytes, mode 0600). Required for
    /// `token_store`; `asc serve token create` makes it when it is missing.
    pub token_key_file: Option<PathBuf>,
}

/// `[web]` (docs/WEB-UI.md): the web UI, served read-only as static files under `/ui`, and the
/// browser-facing RPC (gRPC-Web and Connect unary on the RPC paths). The files are public; every
/// data call the UI makes is an authenticated RPC.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebConfig {
    /// Serve the UI. Off by default. Needs the embedded build (`web/dist` present when asc-serve
    /// was compiled) or `dir`.
    pub enabled: bool,
    /// Serve this build directory instead of the embedded one (development, hotfix). It has the
    /// layout of `web/dist`: `index.html`, `static/`, optional `.br` and `.gz` siblings.
    pub dir: Option<PathBuf>,
    /// Answer gRPC-Web and Connect unary on the RPC paths. Unset, it follows `enabled`; set it to
    /// serve browsers without the UI (`true`) or to keep the UI off the RPC (`false`).
    pub grpc_web: Option<bool>,
    /// Origins other than the server's own that may call the RPC from a browser (`https://app.example`;
    /// http only for loopback). Empty by default: same-origin only. Never a wildcard.
    pub cors_origins: Vec<String>,
    /// Largest gRPC-Web or Connect request body.
    pub max_request: ByteSize,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: None,
            grpc_web: None,
            cors_origins: Vec::new(),
            max_request: ByteSize(ByteSize::MIB),
        }
    }
}

impl WebConfig {
    /// Whether the RPC paths answer browsers: `grpc_web`, which defaults to `enabled`.
    pub fn grpc_web_enabled(&self) -> bool {
        self.grpc_web.unwrap_or(self.enabled)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.dir.is_some() && !self.enabled {
            return invalid("web.dir needs web.enabled = true");
        }
        if !self.cors_origins.is_empty() && !self.grpc_web_enabled() {
            return invalid("web.cors_origins needs web.grpc_web (or web.enabled)");
        }
        for o in &self.cors_origins {
            crate::web::grpc_web::normalise_origin(o)
                .map_err(|e| ConfigError::Invalid(format!("web.cors_origins: {e}")))?;
        }
        if self.max_request.0 == 0 || self.max_request.0 > 64 * ByteSize::MIB {
            return invalid("web.max_request must be between 1 byte and 64MiB");
        }
        Ok(())
    }
}

/// `[[composition_grants]]`: the parent's task tokens may read the child (SPEC §15.5).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionGrant {
    pub parent: String,
    pub child: String,
}

/// `[cloud]` (SPEC §21.3.2, §21.3.7): the per-user caps of user-ref pushes and cloud ingest.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CloudConfig {
    /// User refs one user may hold in one repository (`USER_REFS_LIMIT`).
    pub max_user_refs: usize,
    /// Bytes one user may have charged in one repository (`QUOTA_EXCEEDED`).
    pub max_user_bytes: ByteSize,
}

impl Default for CloudConfig {
    fn default() -> Self {
        let limits = crate::cloud::Limits::default();
        Self {
            max_user_refs: crate::refauthz::DEFAULT_MAX_USER_REFS,
            max_user_bytes: ByteSize(limits.max_user_bytes),
        }
    }
}

impl CloudConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_user_refs == 0 || self.max_user_bytes.0 == 0 {
            return invalid("cloud.max_user_refs and cloud.max_user_bytes must be positive");
        }
        Ok(())
    }

    /// The caps as the quota ledger takes them.
    pub fn limits(&self) -> crate::cloud::Limits {
        crate::cloud::Limits {
            max_user_bytes: self.max_user_bytes.0,
            ..crate::cloud::Limits::default()
        }
    }

    /// The quota every user-ref push and cloud ingest is checked against and charged to.
    pub fn user_quota(&self) -> crate::cloud::quota::UserQuota {
        crate::cloud::quota::UserQuota::new(
            crate::cloud::quota::Quota::local(self.limits()),
            self.max_user_refs,
        )
    }
}

/// The whole `asc-serve.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    pub public_url: Option<String>,
    pub tls: Option<Tls>,
    /// The UDP port where HTTP/3 for `public_url` is served. When set, every response carries
    /// `Alt-Svc: h3=":<port>"` (U89). With `[tls]` and the feature `http3`, this server serves
    /// h3 itself on that port (U89b; `[limits.h3]`); otherwise an h3-capable proxy in front of it
    /// must. Unset sends no header.
    pub http3_port: Option<u16>,
    pub admin: AdminConfig,
    /// `[log]`: text or JSON lines.
    pub log: LogConfig,
    pub role: Role,
    /// Bucket URL: `s3://…`, `file:///…` or `memory:///`.
    pub store: Option<String>,
    /// `postgres://…` or `sqlite:…`.
    pub db: Option<String>,
    /// The upstream tier of a `cache` role.
    pub upstream: Option<String>,
    pub cache: CacheConfig,
    pub auth: AuthConfig,
    #[cfg(feature = "forgejo")]
    pub forgejo: Option<ForgejoConfig>,
    pub turbo: TurboConfig,
    pub mcp: McpConfig,
    pub codeintel: CodeIntelConfig,
    pub bucket: BucketConfig,
    pub limits: Limits,
    /// `[git]` (SPEC §25.9); honoured only with the feature `git-compat`.
    pub git: GitConfig,
    /// `[ssh]` (SPEC §25.11); honoured only with the feature `git-ssh`.
    pub ssh: SshConfig,
    pub gc: crate::gc::GcSettings,
    /// `[cloud]` (SPEC §21.3.7): `max_user_refs` and `max_user_bytes`.
    pub cloud: CloudConfig,
    /// `[web]`: the web UI under `/ui` (docs/WEB-UI.md).
    pub web: WebConfig,
    /// `[vault]`: the master key file for stored secrets.
    pub vault: crate::vault::VaultConfig,
    /// `[netguard]`: the rules for connecting to user-supplied addresses.
    pub netguard: crate::netguard::NetGuardConfig,
    /// `[storage]`: bring-your-own storage limits and timers.
    pub storage: crate::storage::StorageConfig,
    /// `[audit]`: retention, export bound, who may read every scope (docs/BUILDOUT.md section 3).
    pub audit: crate::audit::AuditConfig,
    /// `[ci_insights]`: retention and the flaky window (docs/BUILDOUT.md section 8).
    pub ci_insights: crate::ci::insights::InsightsConfig,
    /// `[secrets]`: bounds of the push-time secret scan (docs/BUILDOUT.md section 4).
    pub secrets: crate::secretscan::Config,
    /// `[signing]`: the server's signing key and the keys per user (docs/BUILDOUT.md section 5).
    pub signing: crate::signing::Config,
    /// `[aikeys]`: bring-your-own AI provider keys (docs/AGENTS.md).
    pub aikeys: crate::ai::AiKeysConfig,
    /// `[agents]`: agent runs, tokens, budgets and the command sandbox (docs/AGENTS.md).
    pub agents: crate::agents::AgentsConfig,
    /// `[workflows]`: repository workflows (docs/WORKFLOWS.md).
    pub workflows: crate::workflows::WorkflowsConfig,
    pub composition_grants: Vec<CompositionGrant>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 7890)),
            public_url: None,
            tls: None,
            http3_port: None,
            admin: AdminConfig::default(),
            log: LogConfig::default(),
            role: Role::Primary,
            store: None,
            db: None,
            upstream: None,
            cache: CacheConfig::default(),
            auth: AuthConfig::default(),
            #[cfg(feature = "forgejo")]
            forgejo: None,
            turbo: TurboConfig::default(),
            mcp: McpConfig::default(),
            codeintel: CodeIntelConfig::default(),
            bucket: BucketConfig::default(),
            limits: Limits::default(),
            git: GitConfig::default(),
            ssh: SshConfig::default(),
            gc: crate::gc::GcSettings::default(),
            cloud: CloudConfig::default(),
            web: WebConfig::default(),
            vault: crate::vault::VaultConfig::default(),
            netguard: crate::netguard::NetGuardConfig::default(),
            storage: crate::storage::StorageConfig::default(),
            audit: crate::audit::AuditConfig::default(),
            ci_insights: crate::ci::insights::InsightsConfig::default(),
            secrets: crate::secretscan::Config::default(),
            signing: crate::signing::Config::default(),
            aikeys: crate::ai::AiKeysConfig::default(),
            agents: crate::agents::AgentsConfig::default(),
            workflows: crate::workflows::WorkflowsConfig::default(),
            composition_grants: Vec::new(),
        }
    }
}

impl ServeConfig {
    /// Reads and validates the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let read = |source| ConfigError::Read {
            path: path.to_owned(),
            source,
        };
        let file = std::fs::File::open(path).map_err(read)?;
        let len = file.metadata().map_err(read)?.len();
        if len > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge(path.to_owned()));
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(file, MAX_CONFIG_BYTES), &mut text)
            .map_err(read)?;
        Self::parse(&text)
    }

    /// Parses and validates config text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks everything the types do not: URL schemes, role requirements, bounds.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.gc.to_config()?;
        if let Some(store) = &self.store {
            let scheme = url_scheme(store);
            if !matches!(scheme, "s3" | "file" | "memory") {
                return invalid("store must be an s3://, file:// or memory:// URL");
            }
            check_store_endpoint(store, Policy::current())?;
        }
        if let Some(db) = &self.db
            && !matches!(url_scheme(db), "postgres" | "postgresql" | "sqlite")
        {
            return invalid("db must be a postgres:// or sqlite: URL");
        }
        match self.role {
            Role::Primary | Role::Indexer => {
                if self.store.is_none() || self.db.is_none() {
                    return invalid("the primary and indexer roles need `store` and `db`");
                }
            }
            Role::Cache => match &self.upstream {
                None => return invalid("the cache role needs `upstream`"),
                Some(u) => check_server_url("upstream", u)?,
            },
        }
        if let Some(url) = &self.public_url {
            check_server_url("public_url", url)?;
        }
        #[cfg(feature = "forgejo")]
        if let Some(forgejo) = &self.forgejo {
            check_server_url("forgejo.url", &forgejo.url)?;
            if forgejo.bridge_promote {
                if forgejo.repo_root.as_ref().is_none_or(|r| !r.has_root()) {
                    return invalid("forgejo.bridge_promote needs an absolute forgejo.repo_root");
                }
                if !self.git.enabled {
                    return invalid("forgejo.bridge_promote needs [git] enabled = true");
                }
                #[cfg(not(feature = "git-compat"))]
                return invalid(
                    "forgejo.bridge_promote needs a build with the `git-compat` feature",
                );
            }
            if self.auth.provider.is_none() {
                return invalid(
                    "[forgejo] does not select the authorizer: set auth.provider to \"forgejo\" or \"token_store\"",
                );
            }
        }
        if self.http3_port == Some(0) {
            return invalid("http3_port must be a UDP port number");
        }
        if !self.admin.listen.ip().is_loopback() && self.admin.token_file.is_none() {
            return invalid("a non-loopback admin.listen needs admin.token_file");
        }
        match self.auth_provider() {
            Some(AuthProvider::TokenStore) if self.auth.token_key_file.is_none() => {
                return invalid("auth.provider = \"token_store\" needs auth.token_key_file");
            }
            #[cfg(feature = "forgejo")]
            Some(AuthProvider::Forgejo) if self.forgejo.is_none() => {
                return invalid("auth.provider = \"forgejo\" needs a [forgejo] section");
            }
            #[cfg(not(feature = "forgejo"))]
            Some(AuthProvider::Forgejo) => {
                return invalid(
                    "auth.provider = \"forgejo\" needs a build with the `forgejo` feature",
                );
            }
            _ => {}
        }
        // The 401 challenge and the metadata document name the resource by absolute URL.
        if self.mcp.enabled && self.public_url.is_none() {
            return invalid("mcp.enabled needs public_url (the resource the 401 challenge names)");
        }
        if let Some(oauth) = &self.mcp.oauth {
            oauth
                .validate()
                .map_err(|e| ConfigError::Invalid(format!("mcp.oauth: {e}")))?;
        }
        if self.mcp.write && !self.mcp.enabled {
            return invalid("mcp.write needs mcp.enabled");
        }
        if self.mcp.max_tokens == 0 || self.mcp.max_inflight_per_principal == 0 {
            return invalid("mcp.max_tokens and mcp.max_inflight_per_principal must be positive");
        }
        self.mcp.validate_exposure()?;
        if self.codeintel.enabled && (self.codeintel.workers == 0 || self.codeintel.memory.0 == 0) {
            return invalid("codeintel.workers and codeintel.memory must be positive");
        }
        if self.bucket.max_concurrency == 0 || self.bucket.timeout_secs == 0 {
            return invalid("bucket.max_concurrency and bucket.timeout_secs must be positive");
        }
        if self.cache.disk.0 > 0 && self.cache.disk_dir.is_none() {
            return invalid("cache.disk needs cache.disk_dir");
        }
        self.ssh.validate()?;
        self.limits.validate()?;
        self.cloud.validate()?;
        self.web.validate()?;
        self.secrets.validate().or_else(invalid)?;
        self.signing.validate().or_else(invalid)?;
        self.audit
            .validate()
            .map_err(|e| ConfigError::Invalid(format!("audit: {e}")))?;
        self.ci_insights
            .validate()
            .map_err(|e| ConfigError::Invalid(format!("ci_insights: {e}")))?;
        self.netguard.validate().or_else(invalid)?;
        self.storage.validate().or_else(invalid)?;
        self.aikeys.validate().or_else(invalid)?;
        self.agents.validate().or_else(invalid)?;
        self.workflows.validate().or_else(invalid)?;
        self.validate_grants()
    }

    /// Whether this server may listen as configured (U89). Without `[tls]` it speaks plain http,
    /// which is only right behind a TLS-terminating proxy: `public_url` must then be the https
    /// URL clients use. Anything else needs the loopback opt-in (`asc.allowLoopbackHttp` or
    /// `ASC_ALLOW_LOOPBACK_HTTP=1`), for local development.
    pub fn validate_listener(&self) -> Result<(), ConfigError> {
        self.check_listener(Policy::current())
    }

    fn check_listener(&self, policy: Policy) -> Result<(), ConfigError> {
        let behind_https_proxy = self
            .public_url
            .as_deref()
            .is_some_and(|u| classify(u, policy) == Ok(Transport::Https));
        if self.tls.is_some() || behind_https_proxy {
            return Ok(());
        }
        if policy.allows_loopback_http() {
            tracing::warn!(
                listen = %self.listen,
                "serving plain http: only for local development (loopback http opt-in is set)"
            );
            return Ok(());
        }
        invalid(format!(
            "asc serve would speak plain http: set [tls], or run behind a TLS-terminating proxy and set public_url to its https URL (for local development on this machine, set {} or {}=1)",
            asc_core::transport_policy::LOOPBACK_CONFIG_KEY,
            asc_core::transport_policy::LOOPBACK_ENV
        ))
    }

    fn validate_grants(&self) -> Result<(), ConfigError> {
        if self.composition_grants.len() > MAX_GRANTS {
            return invalid(format!("more than {MAX_GRANTS} composition grants"));
        }
        let mut seen = HashSet::new();
        for g in &self.composition_grants {
            for name in [&g.parent, &g.child] {
                if RepoName::parse(name).is_err() {
                    return invalid(format!("composition grant names invalid repo {name:?}"));
                }
            }
            if g.parent == g.child {
                return invalid(format!("composition grant {:?} names itself", g.parent));
            }
            if !seen.insert((&g.parent, &g.child)) {
                return invalid(format!(
                    "duplicate composition grant {} -> {}",
                    g.parent, g.child
                ));
            }
        }
        Ok(())
    }

    /// The authorizer `[auth] provider` names; `None` refuses every credential.
    pub fn auth_provider(&self) -> Option<AuthProvider> {
        self.auth.provider
    }

    /// The Forgejo adapter's base URL, when `[forgejo]` is configured. The mirror falls back to it
    /// for repositories without an explicit source.
    pub fn forge_url(&self) -> Option<String> {
        #[cfg(feature = "forgejo")]
        return self.forgejo.as_ref().map(|f| f.url.clone());
        #[cfg(not(feature = "forgejo"))]
        None
    }

    /// The directory `ImportGitPush` may import from, when the host adapter configures one.
    pub fn import_repo_root(&self) -> Option<PathBuf> {
        #[cfg(feature = "forgejo")]
        return self.forgejo.as_ref().and_then(|f| f.repo_root.clone());
        #[cfg(not(feature = "forgejo"))]
        None
    }

    /// The directory whose bare git directories the `bridge_promote` effect keeps current, when
    /// the host adapter asks for it.
    pub fn bridge_promote_root(&self) -> Option<PathBuf> {
        #[cfg(feature = "forgejo")]
        return self
            .forgejo
            .as_ref()
            .filter(|f| f.bridge_promote)
            .and_then(|f| f.repo_root.clone());
        #[cfg(not(feature = "forgejo"))]
        None
    }

    /// Whether `parent`'s task tokens may read `child` (SPEC §15.5).
    pub fn grants_read(&self, parent: &str, child: &str) -> bool {
        self.composition_grants
            .iter()
            .any(|g| g.parent == parent && g.child == child)
    }
}

/// A server URL (`field`) that asc may talk to: https, or plain http to the local machine under
/// the opt-in (U89). The error never repeats credentials, a path or a query.
fn check_server_url(field: &str, url: &str) -> Result<(), ConfigError> {
    match require_network(url, Policy::current()) {
        Ok(Transport::Https | Transport::LoopbackHttp) => Ok(()),
        Ok(_) => invalid(format!("{field} must be an https URL")),
        Err(why) => invalid(format!("{field}: {why}")),
    }
}

/// The `endpoint` of an `s3://` `store` (which the turbo cache shares) follows the same rule as
/// every client URL: https, or plain http to the local machine under the opt-in (U89c). The
/// error names `scheme://host:port` only, never userinfo, a path or the rest of the query.
fn check_store_endpoint(store: &str, policy: Policy) -> Result<(), ConfigError> {
    let Ok(url) = url::Url::parse(store) else {
        return Ok(());
    };
    if url.scheme() != "s3" {
        return Ok(());
    }
    for (k, v) in url.query_pairs() {
        if k == "endpoint" && !v.is_empty() {
            return match require_network(&v, policy) {
                Ok(Transport::Https | Transport::LoopbackHttp) => Ok(()),
                Ok(_) => invalid("store endpoint must be an https URL"),
                Err(why) => invalid(format!("store endpoint: {why}")),
            };
        }
    }
    Ok(())
}

fn url_scheme(s: &str) -> &str {
    s.split_once(':').map_or("", |(scheme, _)| scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A primary config with `store` and `extra` appended.
    fn primary(store: &str, extra: &str) -> Result<ServeConfig, ConfigError> {
        let head = format!("store = \"{store}\"\ndb = \"sqlite::memory:\"\n");
        ServeConfig::parse(&format!("{head}{extra}"))
    }

    #[test]
    fn plain_http_urls_are_refused_in_the_config() {
        for (store, extra) in [
            ("memory:///", "public_url = \"http://asc.example.org\""),
            ("memory:///", "public_url = \"ftp://asc.example.org\""),
            ("memory:///", "[forgejo]\nurl = \"http://forgejo:3000\""),
            (
                "memory:///",
                "[forgejo]\nurl = \"http://user:hunter2@forgejo.example:3000/x\"",
            ),
        ] {
            let err = primary(store, extra).expect_err(extra).to_string();
            assert!(!err.contains("hunter2"), "{err}");
        }
        let cache = |upstream: &str| {
            ServeConfig::parse(&format!("role = \"cache\"\nupstream = \"{upstream}\"\n"))
        };
        assert!(cache("http://primary.example:7890").is_err());
        assert!(cache("https://primary.example:7890").is_ok());
        assert!(primary("memory:///", "public_url = \"https://asc.example.org\"").is_ok());
        assert!(primary("s3://b?endpoint=https://garage:3900", "").is_ok());
    }

    #[test]
    fn log_format_defaults_to_text_and_rejects_unknown_keys() {
        assert_eq!(
            primary("memory:///", "").unwrap().log.format,
            LogFormat::Text
        );
        let json = primary(
            "memory:///",
            "[log]
format = \"json\"",
        )
        .unwrap();
        assert_eq!(json.log.format, LogFormat::Json);
        assert!(
            primary(
                "memory:///",
                "[log]
format = \"xml\""
            )
            .is_err()
        );
        assert!(
            primary(
                "memory:///",
                "[log]
level = \"debug\""
            )
            .is_err()
        );
    }

    #[cfg(feature = "forgejo")]
    #[test]
    fn the_forgejo_adapter_needs_an_explicit_provider() {
        let section = "[forgejo]\nurl = \"https://forgejo.example\"\n";
        // A `[forgejo]` section alone no longer selects the authorizer.
        let err = primary("memory:///", section).unwrap_err().to_string();
        assert!(err.contains("auth.provider"), "{err}");
        let with = |name: &str| {
            primary(
                "memory:///",
                &format!("[auth]\nprovider = \"{name}\"\n{section}"),
            )
            .unwrap()
        };
        let c = with("forgejo");
        assert_eq!(c.auth_provider(), Some(AuthProvider::Forgejo));
        assert_eq!(c.forge_url().as_deref(), Some("https://forgejo.example"));
        // `forge` is the deprecated spelling of the same provider.
        assert_eq!(with("forge").auth_provider(), Some(AuthProvider::Forgejo));
        // The provider needs its section.
        assert!(primary("memory:///", "[auth]\nprovider = \"forgejo\"\n").is_err());
    }

    #[cfg(not(feature = "forgejo"))]
    #[test]
    fn without_the_forgejo_feature_the_forgejo_provider_is_refused() {
        let err = primary("memory:///", "[auth]\nprovider = \"forgejo\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("forgejo"), "{err}");
        assert!(primary("memory:///", "[forgejo]\nurl = \"https://forgejo.example\"").is_err());
    }

    #[test]
    fn unknown_providers_are_refused() {
        assert!(primary("memory:///", "[auth]\nprovider = \"nope\"\n").is_err());
    }

    #[test]
    fn the_store_endpoint_is_https_or_opted_in_loopback() {
        // No exception for the server's own store: plain http to a private host is refused at
        // load, and the error names the origin only.
        let err = primary("s3://b?endpoint=http://u:hunter2@garage-abc:3900/x", "")
            .expect_err("plain http to a private host")
            .to_string();
        assert!(err.contains("http://garage-abc:3900"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");
        assert!(primary("s3://b?endpoint=ftp://garage:3900", "").is_err());
        let loopback = "s3://b?endpoint=http://127.0.0.1:3900";
        assert!(check_store_endpoint(loopback, Policy::STRICT).is_err());
        assert!(check_store_endpoint(loopback, Policy::LOOPBACK_HTTP).is_ok());
        let private = "s3://b?endpoint=http://garage-abc:3900";
        assert!(check_store_endpoint(private, Policy::LOOPBACK_HTTP).is_err());
        assert!(
            check_store_endpoint("s3://b?endpoint=https://garage:3900", Policy::STRICT).is_ok()
        );
        assert!(check_store_endpoint("memory:///", Policy::STRICT).is_ok());
    }

    #[test]
    fn a_plain_http_listener_needs_https_in_front_or_the_dev_opt_in() {
        let cfg = |extra: &str| primary("memory:///", extra).unwrap();
        let proxied_h3 = |port: &str| {
            primary(
                "memory:///",
                &format!(
                    "public_url = \"https://asc.example.org\"
http3_port = {port}"
                ),
            )
        };
        // Behind a TLS-terminating proxy (the Coolify setup): plain listener, https public_url.
        let proxied = cfg("listen = \"0.0.0.0:7890\"\npublic_url = \"https://asc.example.org\"");
        assert!(proxied.check_listener(Policy::STRICT).is_ok());
        // The h3 hint is a port; 0 is not one.
        assert!(proxied_h3("0").is_err());
        assert_eq!(proxied_h3("443").unwrap().http3_port, Some(443));
        // With [tls], http or not.
        let tls = cfg("tls = { cert = \"/c.pem\", key = \"/k.pem\" }");
        assert!(tls.check_listener(Policy::STRICT).is_ok());
        // The dev opt-in is the only way to serve plain http without either.
        let dev = cfg("");
        assert!(dev.check_listener(Policy::STRICT).is_err());
        assert!(dev.check_listener(Policy::LOOPBACK_HTTP).is_ok());
        let msg = dev.check_listener(Policy::STRICT).unwrap_err().to_string();
        assert!(msg.contains("ASC_ALLOW_LOOPBACK_HTTP"), "{msg}");
    }

    const FULL: &str = r#"
listen = "127.0.0.1:7890"
public_url = "https://asc.example.org"
tls = { cert = "/c.pem", key = "/k.pem" }
store = "s3://asc?endpoint=https://garage.internal:3900&region=garage&path_style=true"
db = "postgres://asc_hub@db/forgejo?options=-csearch_path%3Dasc_hub"
cache = { memory = "4GiB", disk_dir = "/var/cache/asc-serve", disk = "50GiB" }
[turbo]
pr_retention_days = 7
[mcp]
enabled = false
[codeintel]
enabled = true
memory = "2GiB"
[codeintel.embed]
device = "cpu"
[limits]
max_inflight = 10
text_memory = "1GiB"
[[composition_grants]]
parent = "asc/dds"
child = "asc/soma"
"#;

    #[test]
    fn web_grpc_web_follows_enabled_and_cors_is_closed_by_default() {
        let parse = |web: &str| {
            ServeConfig::parse(&format!(
                "db = \"sqlite::memory:\"\nstore = \"memory:///\"\n[web]\n{web}"
            ))
        };
        let c = parse("enabled = true\ndir = \"/x\"").unwrap();
        assert!(c.web.grpc_web_enabled());
        assert!(c.web.cors_origins.is_empty());
        assert!(!parse("").unwrap().web.grpc_web_enabled());
        assert!(parse("grpc_web = true").unwrap().web.grpc_web_enabled());
        let off = parse("enabled = true\ngrpc_web = false").unwrap();
        assert!(!off.web.grpc_web_enabled());
        parse("grpc_web = true\ncors_origins = [\"https://app.example\"]").unwrap();
        // CORS needs the switch, and only exact https origins.
        assert!(parse("cors_origins = [\"https://app.example\"]").is_err());
        for bad in ["*", "http://app.example", "https://app.example/x"] {
            let e = parse(&format!("grpc_web = true\ncors_origins = [{bad:?}]"));
            assert!(e.is_err(), "{bad}");
        }
        assert!(parse("grpc_web = true\nmax_request = 0").is_err());
    }

    #[test]
    fn codeintel_embed_dir_is_optional_and_names_the_model_directory() {
        let parse = |embed: &str| {
            ServeConfig::parse(&format!(
                "db = \"sqlite::memory:\"
store = \"memory:///\"
[codeintel]
enabled = true
[codeintel.embed]
{embed}"
            ))
        };
        assert_eq!(parse("").unwrap().codeintel.embed.dir, None);
        let c = parse("dir = \"/opt/asc/embed\"").unwrap();
        assert_eq!(c.codeintel.embed.dir, Some(PathBuf::from("/opt/asc/embed")));
        assert_eq!(c.codeintel.embed.model, "jina-embeddings-v2-base-code");
        assert!(parse("directory = \"/x\"").is_err());
    }

    #[test]
    fn parses_full_spec_example() {
        let c = ServeConfig::parse(FULL).unwrap();
        assert_eq!(c.cache.memory, ByteSize(4 * ByteSize::GIB));
        assert_eq!(c.cache.disk, ByteSize(50 * ByteSize::GIB));
        assert_eq!(c.limits.max_inflight, 10);
        assert_eq!(c.limits.max_inflight_per_token, 64);
        assert_eq!(c.limits.text_memory, ByteSize(ByteSize::GIB));
        assert_eq!(c.turbo.trusted_retention_days, 60);
        assert!(c.grants_read("asc/dds", "asc/soma"));
        assert!(!c.grants_read("asc/soma", "asc/dds"));
        assert_eq!(c.role, Role::Primary);
    }

    #[test]
    fn defaults_match_spec() {
        let c = ServeConfig::parse("store = \"memory:///\"\ndb = \"sqlite::memory:\"").unwrap();
        assert_eq!(c.listen.port(), 7890);
        assert_eq!(c.limits.text_memory, ByteSize(2 * ByteSize::GIB));
        assert_eq!(c.limits.cheap_burst, 400);
        assert_eq!(c.bucket.max_concurrency, 256);
        assert_eq!(c.mcp.max_tokens, 6000);
        assert_eq!(c.admin.listen, "127.0.0.1:9464".parse().unwrap());
    }

    #[test]
    fn admin_listener_off_loopback_needs_a_token_file() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        let open =
            format!("{base}[admin]\nlisten = \"10.0.0.5:9464\"\ntoken_file = \"/etc/asc/a\"");
        assert!(ServeConfig::parse(&open).is_ok());
        assert!(ServeConfig::parse(&format!("{base}[admin]\nlisten = \"[::1]:1\"")).is_ok());
    }

    #[test]
    fn the_mcp_exposure_is_configurable_and_checked() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n\
                    public_url = \"https://asc.example.org\"\n";
        let on = format!("{base}[mcp]\nenabled = true\n");
        let c = ServeConfig::parse(&on).unwrap();
        assert_eq!(
            c.mcp.toolsets,
            ["core", "history", "stack", "projects", "search"]
        );
        assert!(c.mcp.tools.is_none() && c.mcp.allowed_hosts.is_empty() && !c.mcp.forge.enabled());
        let c = ServeConfig::parse(&format!(
            "{on}allowed_hosts = [\"hub.example.org\"]\ntoolsets = [\"core\"]\n\
             tools = [\"log\", \"forge_issue\"]"
        ))
        .unwrap();
        assert_eq!(c.mcp.allowed_hosts, ["hub.example.org"]);
        assert_eq!(c.mcp.toolsets, ["core"]);
        for bad in [
            "allowed_hosts = [\"https://hub.example.org\"]",
            "allowed_hosts = [\"hub.example.org/x\"]",
            "allowed_hosts = [\"*.example.org\"]",
            "allowed_hosts = [\"\"]",
            "toolsets = [\"core\", \"nope\"]",
            "toolsets = [\"write\"]",
            "tools = [\"not_a_tool\"]",
            "[mcp.forge]\nadapter = \"forgejo\"",
            "[mcp.forge]\nadapter = \"github\"",
        ] {
            assert!(
                ServeConfig::parse(&format!("{on}{bad}\n")).is_err(),
                "{bad}"
            );
        }
        // The forge tools need the endpoint itself.
        let forge = "[mcp.forge]\nadapter = \"forgejo\"\nurl = \"https://forge.example.org\"\n\
                     token_file = \"/etc/asc/forge-token\"";
        let off = format!("{base}{forge}");
        assert!(ServeConfig::parse(&off).is_err());
        assert!(ServeConfig::parse(&format!("{on}{forge}")).is_ok());
    }

    #[test]
    fn trusted_proxies_are_cidrs() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        let c = ServeConfig::parse(&format!(
            "{base}[limits]\ntrusted_proxies = [\"10.0.0.0/8\", \"fd00::1\"]"
        ))
        .unwrap();
        assert_eq!(c.limits.trusted_proxies.len(), 2);
        let none = ServeConfig::parse(base).unwrap();
        assert!(none.limits.trusted_proxies.is_empty());
    }

    #[test]
    fn rejects_bad_input() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        for extra in [
            "bogus = 1",
            "[limits]\nmax_inflight = 0",
            "[limits]\ntext_memory = \"12 parsecs\"",
            "[limits]\ntrusted_proxies = [\"not-a-cidr\"]",
            "[mcp]\nenabled = true",
            "[mcp]\nwrite = true",
            "[[composition_grants]]\nparent = \"a/b\"\nchild = \"a/b\"",
            "[[composition_grants]]\nparent = \"../x\"\nchild = \"a/b\"",
            "cache = { disk = \"1GiB\" }",
            "[admin]\nlisten = \"0.0.0.0:9464\"",
            "[admin]\nlisten = \"[::]:9464\"",
        ] {
            assert!(
                ServeConfig::parse(&format!("{base}{extra}")).is_err(),
                "{extra}"
            );
        }
        assert!(ServeConfig::parse("db = \"sqlite::memory:\"").is_err());
        assert!(ServeConfig::parse("store = \"ftp://x\"\ndb = \"sqlite::memory:\"").is_err());
        assert!(ServeConfig::parse("role = \"cache\"").is_err());
        assert!(ServeConfig::parse("role = \"cache\"\nupstream = \"https://p.example\"").is_ok());
    }

    #[test]
    fn git_tables_parse_in_every_build_with_the_spec_defaults() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        let c = ServeConfig::parse(base).unwrap();
        assert!(c.git.enabled);
        assert_eq!(c.git.fsck, GitFsck::Strict);
        assert_eq!(c.git.git_path(), None);
        assert_eq!(c.git.view_dir(&c.cache), None);
        let g = &c.limits.git;
        assert_eq!(g.max_push_bytes, ByteSize(2 * ByteSize::GIB));
        assert_eq!(g.max_push_objects, 2_000_000);
        assert_eq!(g.max_object_bytes, ByteSize(512 * ByteSize::MIB));
        assert_eq!((g.request_timeout, g.idle_timeout), (Secs(900), Secs(60)));
        assert_eq!(g.child_memory, ByteSize(2 * ByteSize::GIB));
        assert_eq!(g.min_free_disk, ByteSize(10 * ByteSize::GIB));
        assert_eq!((g.fetch_per_principal, g.fetch_per_replica), (4, 16));
        assert_eq!((g.push_per_principal, g.push_per_replica), (2, 8));

        let text = format!(
            "{base}cache = {{ disk_dir = \"/var/cache/asc\", disk = \"1GiB\" }}\n[git]\nenabled = false\ngit_path = \"\"\nfsck = \"migration\"\n[limits.git]\nmax_push_bytes = \"4GiB\"\nrequest_timeout = \"30m\"\nidle_timeout = 90\n"
        );
        let c = ServeConfig::parse(&text).unwrap();
        assert!(!c.git.enabled);
        assert_eq!(c.git.fsck, GitFsck::Migration);
        assert_eq!(
            c.git.view_dir(&c.cache),
            Some(PathBuf::from("/var/cache/asc").join("gitview"))
        );
        assert_eq!(c.limits.git.max_push_bytes, ByteSize(4 * ByteSize::GIB));
        assert_eq!(c.limits.git.request_timeout, Secs(1800));
        assert_eq!(c.limits.git.idle_timeout, Secs(90));
        for bad in [
            "[git]\nbogus = 1",
            "[git]\nfsck = \"lax\"",
            "[limits.git]\nmax_push_bytes = 0",
            "[limits.git]\nrequest_timeout = \"soon\"",
            "[limits.git]\nfetch_per_replica = 0",
        ] {
            assert!(
                ServeConfig::parse(&format!("{base}{bad}")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn ssh_tables_parse_in_every_build_with_the_spec_defaults() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        let c = ServeConfig::parse(base).unwrap();
        assert!(!c.ssh.enabled);
        assert_eq!(c.ssh.listen, SocketAddr::from(([0, 0, 0, 0], 2222)));
        assert_eq!(c.ssh.user, "git");
        assert_eq!(c.ssh.min_rsa_bits, 2048);
        assert_eq!(
            c.ssh.host_key_files,
            vec![PathBuf::from(DEFAULT_SSH_HOST_KEY)]
        );
        let l = &c.limits.ssh;
        assert_eq!(
            (l.max_connections, l.max_connections_per_principal),
            (1024, 16)
        );
        assert_eq!(
            (l.max_unauthenticated, l.max_unauthenticated_per_peer),
            (64, 8)
        );
        assert_eq!(
            (l.new_connections_per_sec, l.new_connections_burst),
            (10, 20)
        );
        assert_eq!((l.auth_timeout, l.max_auth_tries), (Secs(30), 6));
        assert_eq!((l.keepalive, l.keepalive_max), (Secs(30), 3));
        assert_eq!((l.max_channels, l.max_running_commands), (4, 2));
        assert_eq!(
            (l.window_size, l.packet_size),
            (ByteSize(2 * ByteSize::MIB), ByteSize(32 * ByteSize::KIB))
        );
        assert_eq!((l.request_timeout, l.idle_timeout), (Secs(900), Secs(60)));

        let text = format!(
            "{base}[ssh]\nenabled = true\nlisten = \"127.0.0.1:22\"\nuser = \"\"\nhost_key_files = [\"a\", \"b\"]\nmin_rsa_bits = 3072\n[limits.ssh]\nmax_auth_tries = 3\nauth_timeout = \"10s\"\n"
        );
        let c = ServeConfig::parse(&text).unwrap();
        assert!(c.ssh.enabled);
        assert_eq!(c.ssh.user, "");
        assert_eq!(c.ssh.min_rsa_bits, 3072);
        assert_eq!(c.limits.ssh.max_auth_tries, 3);
        assert_eq!(c.limits.ssh.auth_timeout, Secs(10));
    }

    #[test]
    fn ssh_settings_out_of_range_are_refused() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        for bad in [
            "[ssh]\nbogus = 1",
            "[ssh]\nmin_rsa_bits = 512",
            "[ssh]\nmin_rsa_bits = 99999",
            "[ssh]\nhost_key_files = []",
            "[ssh]\nuser = \"a b\"",
            "[limits.ssh]\nmax_auth_tries = 0",
            "[limits.ssh]\nmax_auth_tries = 21",
            "[limits.ssh]\nauth_timeout = \"1h\"",
            "[limits.ssh]\nmax_unauthenticated = 2000",
            "[limits.ssh]\nmax_running_commands = 5",
            "[limits.ssh]\npacket_size = \"1MiB\"",
            "[limits.ssh]\nidle_timeout = \"1h\"",
            "[limits.ssh]\nreject_delay_ms = 60000",
        ] {
            assert!(
                ServeConfig::parse(&format!("{base}{bad}")).is_err(),
                "{bad}"
            );
        }
    }

    #[cfg(feature = "git-compat")]
    #[test]
    fn git_limits_above_the_ceilings_or_out_of_order_are_refused() {
        let base = "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n";
        for bad in [
            "[limits.git]\nmax_push_bytes = \"100TiB\"",
            "[limits.git]\nidle_timeout = \"2h\"",
            "[limits.git]\nmax_object_bytes = \"3GiB\"",
        ] {
            assert!(
                ServeConfig::parse(&format!("{base}{bad}")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(ByteSize::parse("512").unwrap(), ByteSize(512));
        assert_eq!(ByteSize::parse("1KiB").unwrap(), ByteSize(1024));
        assert_eq!(ByteSize::parse("3 MB").unwrap(), ByteSize(3_000_000));
        assert!(ByteSize::parse("99999999999TiB").is_err());
        assert!(ByteSize::parse("-1").is_err());
        assert!(ByteSize::parse("GiB").is_err());
    }

    #[test]
    fn load_caps_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("asc-serve.toml");
        std::fs::write(&p, "#".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();
        assert!(matches!(
            ServeConfig::load(&p),
            Err(ConfigError::TooLarge(_))
        ));
        std::fs::write(&p, "store = \"memory:///\"\ndb = \"sqlite::memory:\"").unwrap();
        assert!(ServeConfig::load(&p).is_ok());
        assert!(matches!(
            ServeConfig::load(&dir.path().join("none")),
            Err(ConfigError::Read { .. })
        ));
    }
}
