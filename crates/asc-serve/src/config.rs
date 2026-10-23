//! The `asc-serve.toml` loader (SPEC §8.1, §16.3).
//!
//! The file is operator input but still parsed defensively: it is size-capped, unknown keys are
//! rejected, every count and size is bounded, and secrets are only ever named by file path.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use asc_rpc::server::RepoName;
use serde::{Deserialize, Deserializer};

/// Largest accepted config file.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Most `[[composition_grants]]` entries.
pub const MAX_GRANTS: usize = 10_000;
/// Whether the MCP auth layer exists (M13). Until it does, `mcp.enabled = true` is refused.
pub const MCP_AUTH_LAYER: bool = false;

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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgejoConfig {
    pub url: String,
    pub internal_token_file: Option<PathBuf>,
    pub webhook_secret_file: Option<PathBuf>,
    pub repo_root: Option<PathBuf>,
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
    pub hmac_keys: Vec<PathBuf>,
    pub max_tokens: u32,
    pub max_inflight_per_principal: u32,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            write: false,
            allowed_origins: Vec::new(),
            hmac_keys: Vec::new(),
            max_tokens: 6000,
            max_inflight_per_principal: 8,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbedConfig {
    pub model: String,
    pub device: String,
    pub provider: String,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model: "jina-embeddings-v2-base-code".into(),
            device: "cpu".into(),
            provider: "local".into(),
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
    /// CIDRs (or bare addresses) of reverse proxies in front of this server. Only a connection
    /// from one of them may name the real client in `X-Forwarded-For`; any other peer's header is
    /// ignored. Empty by default.
    pub trusted_proxies: Vec<String>,
    /// `[limits.git]` (SPEC §25.5).
    pub git: GitLimitsConfig,
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
            trusted_proxies: Vec::new(),
            git: GitLimitsConfig::default(),
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
        ];
        if let Some((name, _)) = counts.iter().find(|(_, v)| *v == 0) {
            return invalid(format!("limits.{name} must be greater than zero"));
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
        self.git.validate()
    }
}

/// Which `Authorizer` a server runs (`[auth] provider`, SPEC §15.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthProvider {
    /// Forge delegation (Forgejo today); needs `[forgejo]`.
    Forge,
    /// The standalone token store (M30); needs `auth.token_key_file`.
    TokenStore,
}

/// `[auth]`: the one authorizer for RPC, HTTP routes and MCP.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// `forge` or `token_store`. Absent means `forge` when `[forgejo]` is set, else every
    /// credential is refused.
    pub provider: Option<AuthProvider>,
    /// The file holding the token store's HMAC key (hex, 32 bytes, mode 0600). Required for
    /// `token_store`; `asc serve token create` makes it when it is missing.
    pub token_key_file: Option<PathBuf>,
}

/// `[[composition_grants]]`: the parent's task tokens may read the child (SPEC §15.5).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionGrant {
    pub parent: String,
    pub child: String,
}

/// The whole `asc-serve.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    pub public_url: Option<String>,
    pub tls: Option<Tls>,
    pub admin: AdminConfig,
    pub role: Role,
    /// Bucket URL: `s3://…`, `file:///…` or `memory:///`.
    pub store: Option<String>,
    /// `postgres://…` or `sqlite:…`.
    pub db: Option<String>,
    /// The upstream tier of a `cache` role.
    pub upstream: Option<String>,
    pub cache: CacheConfig,
    pub auth: AuthConfig,
    pub forgejo: Option<ForgejoConfig>,
    pub turbo: TurboConfig,
    pub mcp: McpConfig,
    pub codeintel: CodeIntelConfig,
    pub bucket: BucketConfig,
    pub limits: Limits,
    /// `[git]` (SPEC §25.9); honoured only with the feature `git-compat`.
    pub git: GitConfig,
    pub gc: crate::gc::GcSettings,
    pub composition_grants: Vec<CompositionGrant>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 7890)),
            public_url: None,
            tls: None,
            admin: AdminConfig::default(),
            role: Role::Primary,
            store: None,
            db: None,
            upstream: None,
            cache: CacheConfig::default(),
            auth: AuthConfig::default(),
            forgejo: None,
            turbo: TurboConfig::default(),
            mcp: McpConfig::default(),
            codeintel: CodeIntelConfig::default(),
            bucket: BucketConfig::default(),
            limits: Limits::default(),
            git: GitConfig::default(),
            gc: crate::gc::GcSettings::default(),
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
                Some(u) if !matches!(url_scheme(u), "http" | "https") => {
                    return invalid("upstream must be an http(s) URL");
                }
                Some(_) => {}
            },
        }
        if let Some(url) = &self.public_url
            && !matches!(url_scheme(url), "http" | "https")
        {
            return invalid("public_url must be an http(s) URL");
        }
        if !self.admin.listen.ip().is_loopback() && self.admin.token_file.is_none() {
            return invalid("a non-loopback admin.listen needs admin.token_file");
        }
        match self.auth_provider() {
            Some(AuthProvider::TokenStore) if self.auth.token_key_file.is_none() => {
                return invalid("auth.provider = \"token_store\" needs auth.token_key_file");
            }
            Some(AuthProvider::Forge) if self.forgejo.is_none() => {
                return invalid("auth.provider = \"forge\" needs a [forgejo] section");
            }
            _ => {}
        }
        if self.mcp.enabled && !MCP_AUTH_LAYER {
            return invalid("mcp.enabled = true is refused until the auth layer exists");
        }
        if self.mcp.write && !self.mcp.enabled {
            return invalid("mcp.write needs mcp.enabled");
        }
        if self.mcp.max_tokens == 0 || self.mcp.max_inflight_per_principal == 0 {
            return invalid("mcp.max_tokens and mcp.max_inflight_per_principal must be positive");
        }
        if self.codeintel.enabled && (self.codeintel.workers == 0 || self.codeintel.memory.0 == 0) {
            return invalid("codeintel.workers and codeintel.memory must be positive");
        }
        if self.bucket.max_concurrency == 0 || self.bucket.timeout_secs == 0 {
            return invalid("bucket.max_concurrency and bucket.timeout_secs must be positive");
        }
        if self.cache.disk.0 > 0 && self.cache.disk_dir.is_none() {
            return invalid("cache.disk needs cache.disk_dir");
        }
        self.limits.validate()?;
        self.validate_grants()
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

    /// The effective authorizer: `[auth] provider`, else `forge` when `[forgejo]` is set.
    pub fn auth_provider(&self) -> Option<AuthProvider> {
        self.auth
            .provider
            .or_else(|| self.forgejo.is_some().then_some(AuthProvider::Forge))
    }

    /// Whether `parent`'s task tokens may read `child` (SPEC §15.5).
    pub fn grants_read(&self, parent: &str, child: &str) -> bool {
        self.composition_grants
            .iter()
            .any(|g| g.parent == parent && g.child == child)
    }
}

fn url_scheme(s: &str) -> &str {
    s.split_once(':').map_or("", |(scheme, _)| scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
listen = "127.0.0.1:7890"
public_url = "https://asc.example.org"
tls = { cert = "/c.pem", key = "/k.pem" }
store = "s3://asc?endpoint=https://garage.internal:3900&region=garage&path_style=true"
db = "postgres://asc_hub@db/forgejo?options=-csearch_path%3Dasc_hub"
cache = { memory = "4GiB", disk_dir = "/var/cache/asc-serve", disk = "50GiB" }
[forgejo]
url = "https://forge.example.org"
internal_token_file = "/etc/asc/t"
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
        let open = format!("{base}[admin]\nlisten = \"10.0.0.5:9464\"\ntoken_file = \"/etc/asc/a\"");
        assert!(ServeConfig::parse(&open).is_ok());
        assert!(ServeConfig::parse(&format!("{base}[admin]\nlisten = \"[::1]:1\"")).is_ok());
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
