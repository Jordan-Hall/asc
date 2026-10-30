//! Mirror mode (SPEC §8.6, PLAN U45): the upstream host stays the source of truth and the server
//! follows.
//!
//! A trigger ([`Mirror::trigger`], called by a host adapter's webhook, or the 60 s poll) runs
//! `git fetch --prune` into the bare mirror `<cache disk_dir>/mirrors/<repo-id>.git` through
//! [`RealGit`], imports the new commits incrementally (SPEC §14.1), writes their `cs_graph` rows, and sets the asc refs to
//! the git refs with `via = mirror`, all in one transaction. One importer per repo runs at a time
//! (SPEC §14.4 I6): an in-process mutex, plus `pg_advisory_xact_lock(repo_id)` on Postgres.
//! Imports are idempotent, so duplicate webhooks cost one no-op fetch.
//!
//! The mirror URL, the remote's refs and the webhook body are all untrusted: URLs are checked
//! (no `-` options, no `ext::` helpers, a protocol allow-list), git runs without the host's
//! config, output and run time are bounded, and ref counts are capped. A URL a client supplies
//! (`RepoAdmin.init { mirror_url }`) may only name `https` or `ssh` on the configured Forgejo
//! host or an operator-listed host; local paths and `file:` come from the server config only.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::path::RefName;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_git::import::{ImportError, ImportMap, Importer, Tip};
use asc_git::odb::{Odb, OdbError};
use asc_git::realgit::{RealGit, RealGitError};
use asc_rpc::server::RepoName;
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs};
use async_trait::async_trait;
use base64::Engine as _;
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::ServeConfig;
use crate::graph_sql::{self, NewNode};
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry, RepoMode, SqlGitMap};
use crate::update::{GcLock, PipelineHooks};

/// Most refs one mirror may hold; a bigger remote is refused rather than half-followed.
pub const MAX_MIRROR_REFS: usize = 500_000;
/// Most new changesets whose graph rows one sync records.
const MAX_NEW_CHANGESETS: usize = 2_000_000;
/// Longest accepted `git for-each-ref` output.
const MAX_REF_LIST_BYTES: usize = 128 << 20;
/// Bytes of git's stderr kept for an error message.
const MAX_STDERR_BYTES: usize = 8192;
/// Changeset ids per `cs_graph` lookup while walking new history.
const GRAPH_BATCH: usize = 256;
/// Graph rows inserted per statement group; bounds nothing but keeps progress logs useful.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// What the fetch brings in (SPEC §8.6).
const REFSPECS: [&str; 3] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/pull/*:refs/pull/*",
];
const REF_PREFIXES: [&str; 3] = ["refs/heads/", "refs/tags/", "refs/pull/"];
/// Protocols a fetch may use (`GIT_ALLOW_PROTOCOL`).
const NETWORK_PROTOCOLS: &str = "https:ssh";
/// As [`NETWORK_PROTOCOLS`], for an operator-configured local mirror.
const LOCAL_PROTOCOLS: &str = "https:ssh:file";

/// Why a mirror sync failed.
#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error("repository {0} is not known")]
    NotFound(String),
    #[error("repository {0} is not in mirror mode")]
    NotMirror(String),
    #[error("no mirror source is configured for {0}")]
    NoSource(String),
    #[error("unusable mirror url: {0}")]
    BadUrl(&'static str),
    #[error("git: {0}")]
    Git(String),
    #[error("git timed out")]
    Timeout,
    #[error("the mirror holds more than {MAX_MIRROR_REFS} refs")]
    TooManyRefs,
    #[error("the sync adds more than {MAX_NEW_CHANGESETS} changesets")]
    TooLarge,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    RealGit(#[from] RealGitError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error("import: {0}")]
    Import(#[from] ImportError),
    #[error(transparent)]
    Registry(RegistryError),
    #[error("graph: {0}")]
    Graph(String),
    #[error("refs: {0}")]
    Refs(#[from] RefError),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    /// The repository was deleted or renamed during the sync; nothing was written.
    #[error("the repository changed during the sync: {0}")]
    Stale(String),
}

impl From<RegistryError> for MirrorError {
    fn from(e: RegistryError) -> Self {
        match e {
            RegistryError::NotFound(n) => Self::NotFound(n),
            e => Self::Registry(e),
        }
    }
}

// ------------------------------------------------------------------------------ sources

/// Where one mirror repo is fetched from.
#[derive(Clone)]
pub struct MirrorTarget {
    pub url: String,
    /// Read token, sent as an HTTP basic-auth header through git's environment config (never on
    /// the command line).
    pub token: Option<String>,
}

impl std::fmt::Debug for MirrorTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorTarget")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Who named a mirror URL. Only the operator may name local repositories or any host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// `RepoAdmin.init { mirror_url }`: untrusted.
    Client,
    /// The server config, or the default derived from `forgejo.url`.
    Operator,
}

/// Mirror URLs: client entries (`RepoAdmin.init { mirror_url }` sets them through
/// [`set`](Self::set)), operator entries ([`set_configured`](Self::set_configured)), else
/// `<forgejo.url>/<owner>/<repo>.git`. The two kinds of entry are kept apart because they are
/// checked differently.
#[derive(Default)]
pub struct MirrorSources {
    client: RwLock<HashMap<String, MirrorTarget>>,
    configured: RwLock<HashMap<String, MirrorTarget>>,
    forgejo: Option<String>,
    /// The Forgejo origin (scheme, normalised host, effective port) a client URL must match
    /// exactly; over `ssh` only the default port is allowed on that host.
    origin: Option<(String, String, u16)>,
    /// Hosts the operator listed, each with the one port it may be reached on (`None`: the
    /// default port of the scheme).
    hosts: Vec<(String, Option<u16>)>,
}

impl MirrorSources {
    /// Sources that fall back to the Forgejo at `forgejo_url`, and accept client URLs only on
    /// its host.
    pub fn with_forgejo(forgejo_url: Option<&str>) -> Self {
        Self::with_policy(forgejo_url, &[])
    }

    /// As [`with_forgejo`](Self::with_forgejo), plus `allowed_hosts` (host names or IP literals,
    /// an IPv6 literal in brackets, each optionally with `:port`) that client URLs may also name.
    /// An entry without a port allows the default port of the URL's scheme only. With no Forgejo
    /// and an empty list, every client URL is refused.
    pub fn with_policy(forgejo_url: Option<&str>, allowed_hosts: &[String]) -> Self {
        let origin = forgejo_url
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| {
                Some((
                    u.scheme().to_owned(),
                    u.host()?.to_string(),
                    u.port_or_known_default()?,
                ))
            });
        let mut hosts = Vec::new();
        for h in allowed_hosts {
            let entry = url::Url::parse(&format!("https://{}", h.trim()))
                .ok()
                .filter(|u| u.path() == "/" && u.username().is_empty() && u.query().is_none())
                .and_then(|u| Some((u.host()?.to_string(), u.port())));
            match entry {
                Some(e) => hosts.push(e),
                None => tracing::warn!(host = %h, "ignoring an unusable mirror host"),
            }
        }
        Self {
            client: RwLock::default(),
            configured: RwLock::default(),
            forgejo: forgejo_url.map(|u| u.trim_end_matches('/').to_owned()),
            origin,
            hosts,
        }
    }

    /// Sets the source of `repo` from a client request. Refused unless the URL passes
    /// [`check_client_url`](Self::check_client_url); it is checked again on every use.
    pub fn set(&self, repo: &RepoName, target: MirrorTarget) -> Result<(), MirrorError> {
        self.check_client_url(&target.url)?;
        if let Ok(mut m) = self.client.write() {
            m.insert(repo.to_string(), target);
        }
        Ok(())
    }

    /// Forgets the client-named source of `repo` (its repository was deleted). Operator entries
    /// stay: they are configuration, not part of the repository.
    pub fn remove(&self, repo: &RepoName) {
        if let Ok(mut m) = self.client.write() {
            m.remove(repo.as_str());
        }
    }

    /// Moves the client-named source of `from` to `to` (the repository was renamed). A source
    /// already recorded under `to` is replaced; one under `from` leaves nothing behind.
    pub fn rename(&self, from: &RepoName, to: &RepoName) {
        if let Ok(mut m) = self.client.write() {
            m.remove(to.as_str());
            if let Some(t) = m.remove(from.as_str()) {
                m.insert(to.to_string(), t);
            }
        }
    }

    /// The client-named source of `repo`, if any.
    pub fn client_target(&self, repo: &RepoName) -> Option<MirrorTarget> {
        self.client.read().ok()?.get(repo.as_str()).cloned()
    }

    /// Sets the source of `repo` from the server config: also a local path or `file:` URL, on
    /// any host. Never call this with a value a client chose.
    pub fn set_configured(&self, repo: &RepoName, target: MirrorTarget) -> Result<(), MirrorError> {
        check_url(&target.url)?;
        if let Ok(mut m) = self.configured.write() {
            m.insert(repo.to_string(), target);
        }
        Ok(())
    }

    fn resolve(&self, repo: &RepoName) -> Option<(MirrorTarget, Origin)> {
        for (map, origin) in [
            (&self.configured, Origin::Operator),
            (&self.client, Origin::Client),
        ] {
            if let Ok(m) = map.read()
                && let Some(t) = m.get(repo.as_str())
            {
                return Some((t.clone(), origin));
            }
        }
        self.forgejo.as_ref().map(|base| {
            (
                MirrorTarget {
                    url: format!("{base}/{repo}.git"),
                    token: None,
                },
                Origin::Operator,
            )
        })
    }

    /// Checks `url` for `origin` and returns the protocols the fetch may use.
    fn check(&self, url: &str, origin: Origin) -> Result<&'static str, MirrorError> {
        match origin {
            Origin::Client => self.check_client_url(url).map(|()| NETWORK_PROTOCOLS),
            Origin::Operator => {
                check_url(url)?;
                Ok(if is_local(url) || url.starts_with("file:") {
                    LOCAL_PROTOCOLS
                } else {
                    NETWORK_PROTOCOLS
                })
            }
        }
    }

    /// The policy for a URL a client chose: `https` or `ssh` (no password in the URL) on the
    /// Forgejo origin (scheme, host and port exactly; `ssh` on its default port) or an
    /// operator-listed host on its listed port. An IP literal in a loopback, private, link-local
    /// or unspecified range needs an exact operator entry.
    pub fn check_client_url(&self, url: &str) -> Result<(), MirrorError> {
        let bad = MirrorError::BadUrl;
        check_url(url)?;
        if is_local(url) {
            return Err(bad("local repositories cannot be named by a client"));
        }
        let parsed = url::Url::parse(url).map_err(|_| bad("not a url"))?;
        if !matches!(parsed.scheme(), "https" | "ssh") {
            return Err(bad("protocol not allowed for a client"));
        }
        if parsed.password().is_some() {
            return Err(bad("credentials in the url are not allowed"));
        }
        let host = parsed.host().ok_or_else(|| bad("no host"))?;
        let name = host.to_string();
        let on_forge = self.origin.as_ref().is_some_and(|(_, h, _)| *h == name);
        let on_list = self.hosts.iter().any(|(h, _)| *h == name);
        let listed = on_forge || on_list;
        let internal = match host {
            url::Host::Ipv4(ip) => is_internal(IpAddr::V4(ip)),
            url::Host::Ipv6(ip) => is_internal(IpAddr::V6(ip)),
            url::Host::Domain(_) => false,
        };
        if internal && !listed {
            return Err(bad("internal addresses are not allowed"));
        }
        if !listed {
            return Err(bad(
                "host is not the configured forge host or an allowed mirror host",
            ));
        }
        let scheme = parsed.scheme();
        let default_port = if scheme == "https" { 443 } else { 22 };
        let port = parsed.port().unwrap_or(default_port);
        let forge_ok = self.origin.as_ref().is_some_and(|(s, h, p)| {
            *h == name && ((s == scheme && port == *p) || (scheme == "ssh" && port == 22))
        });
        let list_ok = self
            .hosts
            .iter()
            .any(|(h, p)| *h == name && port == p.unwrap_or(default_port));
        if !forge_ok && !list_ok {
            return Err(bad(
                "the scheme and port are not those of the configured forge or the listed host",
            ));
        }
        Ok(())
    }
}

/// True for an absolute filesystem path (a local repository, or a UNC path on Windows).
fn is_local(url: &str) -> bool {
    Path::new(url).is_absolute() || url.starts_with(['/', '\\'])
}

/// Loopback, private, link-local, unspecified, multicast and shared-address-space addresses.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || (a.octets()[0] == 100 && (64..128).contains(&a.octets()[1]))
        }
        IpAddr::V6(a) => {
            a.to_ipv4_mapped()
                .is_some_and(|v4| is_internal(IpAddr::V4(v4)))
                || a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (a.segments()[0] & 0xfe00) == 0xfc00
                || (a.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Refuses URLs git could be tricked by: option look-alikes, transport helpers (`ext::`), and
/// protocols outside the allow-list (`https`, `ssh`, `file`, or an absolute path for a local
/// repository). This is the check for an operator's URL; a client's also goes through
/// [`MirrorSources::check_client_url`].
pub fn check_url(url: &str) -> Result<(), MirrorError> {
    let bad = MirrorError::BadUrl;
    if url.is_empty() || url.len() > 4096 {
        return Err(bad("empty or too long"));
    }
    if url.starts_with('-') || url.contains(['\0', '\n', '\r']) {
        return Err(bad("looks like an option"));
    }
    if url.contains("::") {
        return Err(bad("transport helpers are not allowed"));
    }
    if Path::new(url).is_absolute() {
        return Ok(());
    }
    let parsed = url::Url::parse(url).map_err(|_| bad("not a url or absolute path"))?;
    if matches!(parsed.scheme(), "https" | "ssh" | "file") {
        Ok(())
    } else {
        Err(bad("protocol not allowed"))
    }
}

// ------------------------------------------------------------------------------ config

/// Mirror settings derived from `asc-serve.toml`.
#[derive(Clone, Debug)]
pub struct MirrorConfig {
    /// `<cache disk_dir>/mirrors`.
    pub dir: PathBuf,
    pub forgejo_url: Option<String>,
    /// Extra hosts a client-supplied mirror URL may name (besides the Forgejo host).
    pub allowed_hosts: Vec<String>,
    pub interval: Duration,
    pub fetch_timeout: Duration,
}

impl MirrorConfig {
    /// `None` when the server has no `cache.disk_dir` to keep mirrors in.
    pub fn from_serve(cfg: &ServeConfig) -> Result<Option<Self>, MirrorError> {
        let Some(disk) = &cfg.cache.disk_dir else {
            return Ok(None);
        };
        Ok(Some(Self {
            dir: disk.join("mirrors"),
            forgejo_url: cfg.forge_url(),
            allowed_hosts: Vec::new(),
            interval: POLL_INTERVAL,
            fetch_timeout: FETCH_TIMEOUT,
        }))
    }
}

// ------------------------------------------------------------------------------ the mirror

/// What one sync did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub commits: usize,
    pub refs_set: usize,
    pub refs_deleted: usize,
    /// Tags and refs asc cannot represent (SPEC §14.1 step 6): expected, not errors.
    pub skipped: Vec<String>,
}

/// The mirror engine: fetch, import and ref sync for mirror-mode repos.
pub struct Mirror {
    registry: Arc<Registry>,
    gc: GcLock,
    git: RealGit,
    cfg: MirrorConfig,
    sources: MirrorSources,
    locks: StdMutex<HashMap<i64, Arc<Mutex<()>>>>,
}

impl Mirror {
    pub fn new(
        registry: Arc<Registry>,
        hooks: &PipelineHooks,
        git: RealGit,
        cfg: MirrorConfig,
    ) -> Self {
        let sources = MirrorSources::with_policy(cfg.forgejo_url.as_deref(), &cfg.allowed_hosts);
        Self {
            registry,
            gc: hooks.gc().clone(),
            git,
            cfg,
            sources,
            locks: StdMutex::default(),
        }
    }

    /// Where mirror repos are fetched from.
    pub fn sources(&self) -> &MirrorSources {
        &self.sources
    }

    fn repo_lock(&self, repo: i64) -> Arc<Mutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, v| Arc::strong_count(v) > 1);
        m.entry(repo).or_default().clone()
    }

    /// Fetches, imports and syncs refs for `name`. Safe to call concurrently and repeatedly.
    pub async fn sync(&self, name: &RepoName) -> Result<SyncReport, MirrorError> {
        let entry = self.registry.open(name).await?;
        if entry.mode != RepoMode::Mirror {
            return Err(MirrorError::NotMirror(name.to_string()));
        }
        let lock = self.repo_lock(entry.key.0);
        let _one = lock.lock().await;
        let result = self.sync_locked(&entry).await;
        if let Err(e) = &result {
            self.record_error(&entry, &e.to_string()).await;
        }
        result
    }

    async fn record_error(&self, entry: &RepoEntry, message: &str) {
        let message: String = message.chars().take(1024).collect();
        let r = sqlx::query(
            "INSERT INTO mirror_state (repo_id, last_import, last_error) VALUES ($1, 0, $2) \
             ON CONFLICT (repo_id) DO UPDATE SET last_error = $2",
        )
        .bind(entry.key.0)
        .bind(message)
        .execute(self.registry.pool())
        .await;
        if let Err(e) = r {
            tracing::warn!(repo = %entry.name, error = %e, "recording the mirror error failed");
        }
    }

    async fn sync_locked(&self, entry: &Arc<RepoEntry>) -> Result<SyncReport, MirrorError> {
        let (target, origin) = self
            .sources
            .resolve(&entry.name)
            .ok_or_else(|| MirrorError::NoSource(entry.name.to_string()))?;
        let protocols = self.sources.check(&target.url, origin)?;
        let pool = self.registry.pool();
        let refs = self.registry.refs();
        // The GC fence (SPEC §3.6) covers the import through the ref commit, like a push.
        self.gc.attach_pool(pool);
        let _gc = self.gc.shared().await?;
        let lock_tx = import_lock(pool, entry.key.0).await?;

        let dir = self.cfg.dir.join(format!("{}.git", entry.key.0));
        self.ensure_repo(&dir).await?;
        self.fetch(&dir, &target, protocols).await?;
        let tips = self.list_refs(&dir).await?;

        let report = import_tips(entry.clone(), pool.clone(), dir, tips).await?;
        let desired = desired_refs(&report);
        let nodes = new_graph_nodes(entry, pool, &report).await?;
        let (ref_txn, set, deleted) = self.plan_refs(entry, &desired).await?;

        let mut tx = match lock_tx {
            Some(t) => t,
            None => refs.begin().await?,
        };
        self.registry
            .guard(&mut tx, entry)
            .await
            .map_err(|s| MirrorError::Stale(s.message().to_owned()))?;
        let new_nodes: Vec<NewNode<'_>> = nodes
            .iter()
            .map(|n| NewNode {
                id: n.id,
                generation: n.generation,
                parents: &n.parents,
            })
            .collect();
        graph_sql::insert_nodes(&mut tx, entry.ns, &new_nodes).await?;
        if let Some(txn) = &ref_txn {
            refs.update_in_tx(&mut tx, entry.key, txn).await?;
        }
        sqlx::query(
            "INSERT INTO mirror_state (repo_id, last_import, last_error) VALUES ($1, $2, NULL) \
             ON CONFLICT (repo_id) DO UPDATE SET last_import = $2, last_error = NULL",
        )
        .bind(entry.key.0)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SyncReport {
            commits: report.commits,
            refs_set: set,
            refs_deleted: deleted,
            skipped: report
                .skipped_refs
                .iter()
                .map(asc_git::import::SkippedRef::warning)
                .collect(),
        })
    }

    /// The ref changes that make the asc refs under the mirrored prefixes equal `desired`. Read
    /// before the write transaction opens (a one-connection pool could not read inside it).
    async fn plan_refs(
        &self,
        entry: &RepoEntry,
        desired: &HashMap<RefName, RefTarget>,
    ) -> Result<(Option<RefTxn>, usize, usize), MirrorError> {
        let refs = self.registry.refs();
        let mut current = HashMap::new();
        for prefix in REF_PREFIXES {
            for e in refs.list(entry.key, prefix).await? {
                current.insert(e.name, e.target);
            }
        }
        let mut updates = Vec::new();
        let (mut set, mut deleted) = (0, 0);
        for (name, target) in desired {
            if current.get(name) == Some(target) {
                continue;
            }
            set += 1;
            updates.push(RefUpdate {
                name: name.clone(),
                expect: current
                    .get(name)
                    .map_or(Expect::Absent, |t| Expect::Value(*t)),
                new: Some(*target),
                expires_at: None,
            });
        }
        for (name, target) in &current {
            if !desired.contains_key(name) {
                deleted += 1;
                updates.push(RefUpdate {
                    name: name.clone(),
                    expect: Expect::Value(*target),
                    new: None,
                    expires_at: None,
                });
            }
        }
        if updates.is_empty() {
            return Ok((None, 0, 0));
        }
        updates.sort_by(|a, b| a.name.cmp(&b.name));
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: "mirror".into(),
                via: "mirror".into(),
            },
            reason: "mirror sync".into(),
            op_id: Uuid::new_v4(),
        };
        Ok((Some(txn), set, deleted))
    }

    // -------------------------------------------------------------------------- git

    fn base(&self, dir: &Path, protocols: &str) -> Result<tokio::process::Command, MirrorError> {
        let mut cmd = tokio::process::Command::from(self.git.command()?);
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        cmd.arg("--git-dir")
            .arg(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_ALLOW_PROTOCOL", protocols)
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Ok(cmd)
    }

    async fn ensure_repo(&self, dir: &Path) -> Result<(), MirrorError> {
        if dir.join("HEAD").is_file() {
            return Ok(());
        }
        std::fs::create_dir_all(dir)?;
        let mut cmd = tokio::process::Command::from(self.git.command()?);
        cmd.arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg("--initial-branch=master")
            .arg(dir);
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        let mut args = cmd;
        run_capped(&mut args, 1024, self.cfg.fetch_timeout).await?;
        Ok(())
    }

    /// The `git fetch` command for `target`, with `protocols` as `GIT_ALLOW_PROTOCOL`.
    fn fetch_command(
        &self,
        dir: &Path,
        target: &MirrorTarget,
        protocols: &str,
    ) -> Result<tokio::process::Command, MirrorError> {
        let mut cmd = self.base(dir, protocols)?;
        if let Some(token) = &target.token {
            let basic =
                base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
        }
        cmd.args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
            .args(["-c", "protocol.ext.allow=never"])
            // A redirect must not carry the token, or the fetch, to a host the checks never saw.
            .args(["-c", "http.followRedirects=false"])
            .args(["-c", "credential.helper=", "-c", "core.askPass="])
            .args([
                "fetch",
                "--quiet",
                "--prune",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--",
            ])
            .arg(&target.url)
            .args(REFSPECS);
        Ok(cmd)
    }

    async fn fetch(
        &self,
        dir: &Path,
        target: &MirrorTarget,
        protocols: &str,
    ) -> Result<(), MirrorError> {
        let mut cmd = self.fetch_command(dir, target, protocols)?;
        run_capped(&mut cmd, 1024, self.cfg.fetch_timeout).await?;
        Ok(())
    }

    async fn list_refs(&self, dir: &Path) -> Result<Vec<Tip>, MirrorError> {
        let mut cmd = self.base(dir, LOCAL_PROTOCOLS)?;
        cmd.args(["for-each-ref", "--format=%(objectname) %(refname)"])
            .args(REF_PREFIXES.map(|p| p.trim_end_matches('/')));
        let out = run_capped(&mut cmd, MAX_REF_LIST_BYTES, self.cfg.fetch_timeout).await?;
        let mut tips = Vec::new();
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let Some(sp) = line.iter().position(|&b| b == b' ') else {
                return Err(MirrorError::Git("unexpected for-each-ref output".into()));
            };
            let (oid, name) = (&line[..sp], &line[sp + 1..]);
            let oid: GitOid = std::str::from_utf8(oid)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| MirrorError::Git("unexpected object id in for-each-ref".into()))?;
            if tips.len() >= MAX_MIRROR_REFS {
                return Err(MirrorError::TooManyRefs);
            }
            tips.push(Tip {
                name: name.to_vec(),
                oid,
            });
        }
        Ok(tips)
    }

    // -------------------------------------------------------------------------- triggers

    /// Syncs `repo` once on behalf of an outside trigger (a host adapter's push webhook). The sync
    /// runs as its own task, so it outlives a caller that hangs up. Duplicate triggers cost one
    /// no-op fetch.
    pub async fn trigger(self: &Arc<Self>, repo: &RepoName) -> Result<SyncReport, MirrorError> {
        let me = self.clone();
        let repo = repo.clone();
        tokio::spawn(async move { me.sync(&repo).await })
            .await
            .map_err(|e| MirrorError::Git(format!("sync task: {e}")))?
    }

    /// Syncs every mirror repo once.
    pub async fn poll_once(&self) {
        let rows = sqlx::query("SELECT name FROM repos WHERE mode = 'mirror' ORDER BY id")
            .fetch_all(self.registry.pool())
            .await;
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "listing mirror repos failed");
                return;
            }
        };
        for row in rows {
            let Ok(name) = row.try_get::<String, _>("name") else {
                continue;
            };
            let Ok(name) = RepoName::parse(&name) else {
                continue;
            };
            if let Err(e) = self.sync(&name).await {
                tracing::warn!(repo = %name, error = %e, "mirror poll failed");
            }
        }
    }

    /// Polls every `interval`; abort the handle to stop (a sync interrupted mid-way is safe).
    pub fn spawn_poller(self: &Arc<Self>) -> JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(me.cfg.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                me.poll_once().await;
            }
        })
    }
}

/// Runs `cmd`, returns stdout capped at `cap` bytes (more is an error), and fails on a non-zero
/// exit with the head of stderr. Both pipes are drained so the child never blocks.
async fn run_capped(
    cmd: &mut tokio::process::Command,
    cap: usize,
    timeout: Duration,
) -> Result<Vec<u8>, MirrorError> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(MirrorError::Git("no pipes".into()));
    };
    let run = async {
        let (stdout, stderr, status) = tokio::join!(
            read_capped(out, cap),
            read_capped(err, MAX_STDERR_BYTES),
            child.wait()
        );
        Ok::<_, std::io::Error>((stdout?, stderr?, status?))
    };
    let ((stdout, over), (stderr, _), status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| MirrorError::Timeout)??;
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr);
        return Err(MirrorError::Git(format!("{status}: {}", text.trim())));
    }
    if over {
        return Err(MirrorError::Git("output is too large".into()));
    }
    Ok(stdout)
}

/// Reads up to `cap` bytes, then discards the rest. The flag says whether there was more.
async fn read_capped<R: AsyncRead + Unpin>(
    mut r: R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    (&mut r).take(cap as u64 + 1).read_to_end(&mut buf).await?;
    let over = buf.len() > cap;
    buf.truncate(cap);
    if over {
        tokio::io::copy(&mut r, &mut tokio::io::sink()).await?;
    }
    Ok((buf, over))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ------------------------------------------------------------------------------ locking

/// Serialises importers of one repo across processes (SPEC §14.4 I6): on Postgres, a transaction
/// holding `pg_advisory_xact_lock(repo_id)` that the caller commits together with its writes;
/// `None` on SQLite, whose single writer already serialises. `ImportGitPush` (U48d) takes the same
/// lock. Callers in one process also hold a per-repo mutex, since pool connections do not queue.
pub async fn import_lock(
    pool: &AnyPool,
    repo_id: i64,
) -> Result<Option<Transaction<'static, Any>>, sqlx::Error> {
    if !matches!(
        pool.connect_options().database_url.scheme(),
        "postgres" | "postgresql"
    ) {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    Ok(Some(tx))
}

// ------------------------------------------------------------------------------ import glue

/// The `git_map` table as the importer's lookup and sink.
struct SqlImportMap {
    pool: AnyPool,
    ns: crate::registry::Ns,
    sink: Arc<SqlGitMap>,
}

impl SqlImportMap {
    fn new(pool: AnyPool, entry: &RepoEntry) -> Self {
        Self {
            pool,
            ns: entry.ns,
            sink: entry.git_map.clone(),
        }
    }
}

#[async_trait]
impl GitMapSink for SqlImportMap {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        self.sink.put(rows).await
    }
}

#[async_trait]
impl ImportMap for SqlImportMap {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        let code: i32 = match kind {
            ObjectKind::Blob => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Commit => 3,
            ObjectKind::Tag => 4,
        };
        let sink = |e: sqlx::Error| derive::Error::Sink(e.to_string());
        let row = sqlx::query("SELECT asc FROM git_map WHERE ns = $1 AND git = $2 AND kind = $3")
            .bind(self.ns.as_bytes().to_vec())
            .bind(git.as_bytes().to_vec())
            .bind(code)
            .fetch_optional(&self.pool)
            .await
            .map_err(sink)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let asc: Vec<u8> = row.try_get("asc").map_err(sink)?;
        <[u8; 32]>::try_from(asc.as_slice())
            .map(Some)
            .map_err(|_| derive::Error::Sink("git_map holds an asc id that is not 32 bytes".into()))
    }
}

/// Imports `tips` from the git repo at `dir`. The importer holds git's single-threaded object
/// database handles, so it runs on a blocking thread rather than on the shared runtime workers.
async fn import_tips(
    entry: Arc<RepoEntry>,
    pool: AnyPool,
    dir: PathBuf,
    tips: Vec<Tip>,
) -> Result<asc_git::import::ImportReport, MirrorError> {
    let handle = tokio::runtime::Handle::current();
    let joined = tokio::task::spawn_blocking(move || {
        handle.block_on(async {
            let odb = Odb::open(&dir)?;
            let map = SqlImportMap::new(pool, &entry);
            let mut importer = Importer::new(
                entry.backend.engine(),
                entry.objects.clone(),
                &odb,
                &map,
                entry.import,
            );
            Ok::<_, MirrorError>(importer.import_commits(&tips).await?)
        })
    })
    .await;
    joined.map_err(|e| MirrorError::Git(format!("import task: {e}")))?
}

/// The asc ref value for each mirrored git ref: an annotated tag names its `TagRecord`, anything
/// else its changeset. Names outside the mirrored prefixes or not UTF-8 are dropped.
fn desired_refs(report: &asc_git::import::ImportReport) -> HashMap<RefName, RefTarget> {
    let mut out = HashMap::new();
    let named = report
        .refs
        .iter()
        .map(|(n, c)| (n, RefTarget::Changeset(*c)))
        .chain(report.tags.iter().map(|(n, t)| (n, RefTarget::Tag(*t))));
    for (name, target) in named {
        let Ok(text) = std::str::from_utf8(name) else {
            continue;
        };
        if !REF_PREFIXES.iter().any(|p| text.starts_with(p)) {
            continue;
        }
        if let Ok(name) = RefName::new(name.clone()) {
            // Tags come last, so a tag ref ends up naming its `TagRecord`.
            out.insert(name, target);
        }
    }
    out
}

/// One `cs_graph` row to add.
struct GraphNode {
    id: ChangesetId,
    generation: u32,
    parents: Vec<ChangesetId>,
}

/// The imported changesets that `cs_graph` does not hold yet, parents first, with generations.
async fn new_graph_nodes(
    entry: &RepoEntry,
    pool: &AnyPool,
    report: &asc_git::import::ImportReport,
) -> Result<Vec<GraphNode>, MirrorError> {
    let graph = |e: &dyn std::fmt::Display| MirrorError::Graph(e.to_string());
    let engine = entry.backend.engine();
    let mut seen: HashSet<ChangesetId> = report.refs.iter().map(|(_, c)| *c).collect();
    let mut frontier: Vec<ChangesetId> = seen.iter().copied().collect();
    let mut parents_of: HashMap<ChangesetId, Vec<ChangesetId>> = HashMap::new();
    let mut gens: HashMap<ChangesetId, u32> = HashMap::new();
    while !frontier.is_empty() {
        let at = frontier.len().saturating_sub(GRAPH_BATCH);
        let batch = frontier.split_off(at);
        let known = graph_sql::known(pool, entry.ns, &batch)
            .await
            .map_err(|e| graph(&e))?;
        for id in batch {
            if let Some(&g) = known.get(&id) {
                gens.insert(id, g);
                continue;
            }
            if parents_of.len() >= MAX_NEW_CHANGESETS {
                return Err(MirrorError::TooLarge);
            }
            let cs = engine.get_changeset(id).await.map_err(|e| graph(&e))?;
            for p in &cs.parents {
                if seen.insert(*p) {
                    frontier.push(*p);
                }
            }
            parents_of.insert(id, cs.parents.clone());
        }
    }
    // Iterative post-order: a node is numbered once all its parents are.
    let mut out = Vec::with_capacity(parents_of.len());
    let mut entered: HashSet<ChangesetId> = HashSet::new();
    let roots: Vec<ChangesetId> = parents_of.keys().copied().collect();
    for root in roots {
        let mut stack = vec![(root, false)];
        while let Some((id, done)) = stack.pop() {
            if gens.contains_key(&id) {
                continue;
            }
            let parents = parents_of
                .get(&id)
                .ok_or_else(|| MirrorError::Graph("a parent is missing".into()))?;
            if !done {
                if entered.insert(id) {
                    stack.push((id, true));
                    stack.extend(parents.iter().map(|p| (*p, false)));
                }
                continue;
            }
            let mut top = 0u32;
            for p in parents {
                let g = gens
                    .get(p)
                    .ok_or_else(|| MirrorError::Graph("the history has a cycle".into()))?;
                top = top.max(*g);
            }
            let generation = top.saturating_add(1);
            gens.insert(id, generation);
            out.push(GraphNode {
                id,
                generation,
                parents: parents.clone(),
            });
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ registration

/// The one registration line of this unit (called from `lib.rs`). A host adapter that wants push
/// webhooks routes them to [`Mirror::trigger`]; the caller starts [`Mirror::spawn_poller`] on the
/// result. Returns `None` (mirror mode off) without `cache.disk_dir` or without a real git.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    cfg: &ServeConfig,
) -> Result<Option<Arc<Mirror>>, MirrorError> {
    let Some(mc) = MirrorConfig::from_serve(cfg)? else {
        return Ok(None);
    };
    let git = match RealGit::resolve(None, None) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "mirror mode is off: no usable git");
            return Ok(None);
        }
    };
    Ok(Some(Arc::new(Mirror::new(
        registry.clone(),
        &ext.pipeline,
        git,
        mc,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::testutil::registry;
    use crate::registry::NewRepo;

    fn git(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
        use std::io::Write as _;
        let real = RealGit::resolve(None, None).expect("git is installed");
        let mut cmd = real.command().unwrap();
        cmd.arg("--git-dir")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Commits `file = content` on top of `parent` and points `refname` at it.
    fn push(up: &Path, parent: Option<&str>, file: &str, content: &str, refname: &str) -> String {
        let blob = git(up, &["hash-object", "-w", "--stdin"], content.as_bytes());
        let tree = git(
            up,
            &["mktree"],
            format!("100644 blob {blob}\t{file}\n").as_bytes(),
        );
        let mut args = vec!["commit-tree", tree.as_str(), "-m", content];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        let commit = git(up, &args, b"");
        git(up, &["update-ref", refname, &commit], b"");
        commit
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        up: PathBuf,
        registry: Arc<Registry>,
        entry: Arc<RepoEntry>,
        mirror: Arc<Mirror>,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let up = tmp.path().join("up.git");
        let real = RealGit::resolve(None, None).expect("git is installed");
        let ok = real
            .command()
            .unwrap()
            .args(["init", "--bare", "--quiet", "--initial-branch=master"])
            .arg(&up)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let registry = registry().await;
        let name = RepoName::parse("o/m").unwrap();
        let entry = registry
            .create(
                &name,
                &NewRepo {
                    mode: RepoMode::Mirror,
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        let cfg = MirrorConfig {
            dir: tmp.path().join("mirrors"),
            forgejo_url: None,
            allowed_hosts: Vec::new(),
            interval: POLL_INTERVAL,
            fetch_timeout: Duration::from_secs(120),
        };
        let mirror = Arc::new(Mirror::new(
            registry.clone(),
            &PipelineHooks::default(),
            real,
            cfg,
        ));
        // A local upstream is operator-sourced; a client could not name it.
        mirror
            .sources()
            .set_configured(
                &name,
                MirrorTarget {
                    url: up.to_str().unwrap().to_owned(),
                    token: None,
                },
            )
            .unwrap();
        Fixture {
            _tmp: tmp,
            up,
            registry,
            entry,
            mirror,
        }
    }

    /// The git refs of the upstream, name to oid.
    fn git_refs(up: &Path) -> Vec<(String, String)> {
        git(
            up,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
            b"",
        )
        .lines()
        .map(|l| {
            let (o, n) = l.split_once(' ').unwrap();
            (n.to_owned(), o.to_owned())
        })
        .collect()
    }

    /// Every asc ref under the mirrored prefixes, name to the git oid of its peeled commit.
    async fn asc_refs(f: &Fixture) -> Vec<(String, String)> {
        let engine = f.entry.backend.engine();
        let mut out = Vec::new();
        for prefix in REF_PREFIXES {
            for e in f.registry.refs().list(f.entry.key, prefix).await.unwrap() {
                let cs = match e.target {
                    RefTarget::Changeset(c) => c,
                    RefTarget::Tag(_) => {
                        // Resolve through the git map: the tag's commit is what `peel` names.
                        let id = f
                            .registry
                            .refs()
                            .get(f.entry.key, &e.name)
                            .await
                            .unwrap()
                            .unwrap();
                        assert!(matches!(id.target, RefTarget::Tag(_)));
                        out.push((e.name.to_string(), String::new()));
                        continue;
                    }
                };
                out.push((
                    e.name.to_string(),
                    engine.git_commit_of(cs).await.unwrap().to_string(),
                ));
            }
        }
        out.sort();
        out
    }

    async fn count(f: &Fixture, sql: &str) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(f.registry.pool())
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    #[test]
    fn urls_are_checked() {
        for bad in [
            "",
            "-oProxyCommand=x",
            "ext::sh -c id",
            "fd::7",
            "ftp://h/r.git",
            "http://h/r.git",
            "git://h/r.git",
            "relative/path",
            "https://h/r\n.git",
        ] {
            assert!(check_url(bad).is_err(), "{bad:?}");
        }
        for good in [
            "https://forgejo.example/o/r.git",
            "ssh://git@h/o/r.git",
            "file:///srv/r.git",
            std::env::temp_dir().to_str().unwrap(),
        ] {
            assert!(check_url(good).is_ok(), "{good:?}");
        }
    }

    fn policy() -> MirrorSources {
        MirrorSources::with_policy(
            Some("https://Forgejo.example:3000/"),
            &["extra.example".to_owned(), "10.1.2.3".to_owned()],
        )
    }

    fn target(url: &str) -> MirrorTarget {
        MirrorTarget {
            url: url.to_owned(),
            token: None,
        }
    }

    #[test]
    fn client_urls_are_refused_unless_https_or_ssh_on_an_allowed_host() {
        let s = policy();
        let name = RepoName::parse("o/r").unwrap();
        let tmp = std::env::temp_dir();
        for bad in [
            tmp.to_str().unwrap(),
            "/srv/r.git",
            "file:///srv/r.git",
            r"\\attacker\share\r.git",
            "//attacker/share/r.git",
            "http://forgejo.example/o/r.git",
            "git://forgejo.example/o/r.git",
            "https://elsewhere.example/o/r.git",
            "https://forgejo.example.evil.test/o/r.git",
            "https://127.0.0.1/o/r.git",
            "https://[::1]/o/r.git",
            "https://169.254.169.254/latest",
            "https://192.168.0.5/o/r.git",
            "https://0.0.0.0/o/r.git",
            "https://2130706433/o/r.git",
            "https://user:pw@forgejo.example:3000/o/r.git",
            "ssh://git:pw@forgejo.example/o/r.git",
            // The forge origin is exact: another port of the forge host, or the default one, is not it.
            "https://forgejo.example/o/r.git",
            "https://forgejo.example:8443/o/r.git",
            "https://forgejo.example:22/o/r.git",
            "ssh://git@forgejo.example:2222/o/r.git",
            "ssh://git@forgejo.example:3000/o/r.git",
            // A listed host without a port takes the default port of the scheme only.
            "https://extra.example:8443/o/r.git",
            "ssh://git@extra.example:2222/o/r.git",
        ] {
            assert!(s.check_client_url(bad).is_err(), "{bad:?}");
            assert!(s.set(&name, target(bad)).is_err(), "{bad:?}");
        }
        assert!(s.resolve(&RepoName::parse("o/none").unwrap()).is_some());
        // With no Forgejo and no allow-list, every client URL is refused.
        let none = MirrorSources::with_forgejo(None);
        assert!(
            none.check_client_url("https://forgejo.example/o/r.git")
                .is_err()
        );
    }

    #[test]
    fn client_urls_on_the_forgejo_host_or_the_allow_list_are_accepted() {
        let s = policy();
        for good in [
            "https://FORGEJO.example:3000/o/r.git",
            "ssh://git@forgejo.example/o/r.git",
            "https://extra.example/o/r.git",
            // An operator-listed internal literal is allowed; an unlisted one is not.
            "https://10.1.2.3/o/r.git",
        ] {
            assert!(s.check_client_url(good).is_ok(), "{good:?}");
        }
        assert!(s.check_client_url("https://10.1.2.4/o/r.git").is_err());
        // An entry with a port allows that port and no other.
        let ported = MirrorSources::with_policy(None, &["git.example:8443".to_owned()]);
        assert!(
            ported
                .check_client_url("https://git.example:8443/o/r.git")
                .is_ok()
        );
        assert!(
            ported
                .check_client_url("https://git.example/o/r.git")
                .is_err()
        );
        assert!(
            ported
                .check_client_url("https://git.example:9/o/r.git")
                .is_err()
        );
        let name = RepoName::parse("o/r").unwrap();
        s.set(&name, target("https://forgejo.example:3000/o/r.git"))
            .unwrap();
        let (t, origin) = s.resolve(&name).unwrap();
        assert_eq!(t.url, "https://forgejo.example:3000/o/r.git");
        assert_eq!(origin, Origin::Client);
        assert_eq!(s.check(&t.url, origin).unwrap(), NETWORK_PROTOCOLS);
    }

    #[test]
    fn operator_sources_may_be_local_and_choose_the_fetch_protocols() {
        let s = policy();
        let name = RepoName::parse("o/r").unwrap();
        let local = std::env::temp_dir().to_str().unwrap().to_owned();
        s.set_configured(&name, target(&local)).unwrap();
        let (t, origin) = s.resolve(&name).unwrap();
        assert_eq!((t.url.as_str(), origin), (local.as_str(), Origin::Operator));
        assert_eq!(s.check(&t.url, origin).unwrap(), LOCAL_PROTOCOLS);
        assert_eq!(
            s.check("file:///srv/r.git", Origin::Operator).unwrap(),
            LOCAL_PROTOCOLS
        );
        // The same path from a client is refused.
        assert!(s.check(&local, Origin::Client).is_err());
        // The default derived from the Forgejo URL is operator-sourced and https only.
        let other = RepoName::parse("o/other").unwrap();
        let (d, origin) = s.resolve(&other).unwrap();
        assert_eq!(d.url, "https://Forgejo.example:3000/o/other.git");
        assert_eq!(s.check(&d.url, origin).unwrap(), NETWORK_PROTOCOLS);
        // The operator entry still goes through the syntax check.
        assert!(s.set_configured(&name, target("-x")).is_err());
    }

    #[tokio::test]
    async fn the_fetch_command_refuses_redirects_and_prompts() {
        let f = fixture().await;
        let target = MirrorTarget {
            url: "https://forgejo.example/o/r.git".into(),
            token: Some("secret-token".into()),
        };
        let cmd = f
            .mirror
            .fetch_command(Path::new("m.git"), &target, NETWORK_PROTOCOLS)
            .unwrap();
        let cmd = cmd.as_std();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has = |a: &str| args.iter().any(|x| x == a);
        assert!(has("http.followRedirects=false"), "{args:?}");
        assert!(has("credential.helper="), "{args:?}");
        let env = |k: &str| {
            cmd.get_envs()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(env("GIT_ALLOW_PROTOCOL"), Some(Some("https:ssh".into())));
        assert_eq!(env("GIT_TERMINAL_PROMPT"), Some(Some("0".into())));
        assert_eq!(env("GIT_ASKPASS"), Some(None));
        // The token travels in the environment, never on the command line.
        assert!(args.iter().all(|a| !a.contains("secret-token")));
    }

    #[tokio::test]
    async fn trigger_follows_the_upstream_and_duplicates_are_harmless() {
        let m = RepoName::parse("o/m").unwrap();
        let f = fixture().await;
        let c1 = push(&f.up, None, "a.txt", "one\n", "refs/heads/master");
        push(&f.up, Some(&c1), "b.txt", "two\n", "refs/heads/feature");
        git(&f.up, &["tag", "-a", "-m", "release", "v1", &c1], b"");
        git(&f.up, &["update-ref", "refs/pull/1/head", &c1], b"");

        f.mirror.trigger(&m).await.unwrap();
        let got = asc_refs(&f).await;
        let mut want: Vec<_> = git_refs(&f.up)
            .into_iter()
            // An annotated tag's asc ref names its `TagRecord`, not a commit.
            .map(|(n, o)| {
                if n == "refs/tags/v1" {
                    (n, String::new())
                } else {
                    (n, o)
                }
            })
            .collect();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM cs_graph").await, 2);
        let state =
            "SELECT COUNT(*) FROM mirror_state WHERE last_error IS NULL AND last_import > 0";
        assert_eq!(count(&f, state).await, 1);
        let logged = count(&f, "SELECT COUNT(*) FROM ref_log").await;
        assert_eq!(logged, 4);

        // The same webhook again changes nothing.
        f.mirror.trigger(&m).await.unwrap();
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, logged);

        // A move, a new commit and a deletion all arrive; the ref log says `mirror`.
        let c3 = push(&f.up, Some(&c1), "c.txt", "three\n", "refs/heads/master");
        git(&f.up, &["update-ref", "-d", "refs/heads/feature"], b"");
        f.mirror.trigger(&m).await.unwrap();
        let got = asc_refs(&f).await;
        assert!(got.contains(&("refs/heads/master".to_owned(), c3)));
        assert!(!got.iter().any(|(n, _)| n == "refs/heads/feature"));
        assert_eq!(count(&f, "SELECT COUNT(*) FROM cs_graph").await, 3);
        assert_eq!(
            count(&f, "SELECT COUNT(*) FROM ref_log WHERE via <> 'mirror'").await,
            0
        );
    }

    #[tokio::test]
    async fn trigger_refuses_unknown_and_primary_repos() {
        let f = fixture().await;
        push(&f.up, None, "a", "x", "refs/heads/master");
        let unknown = RepoName::parse("o/zzz").unwrap();
        assert!(matches!(
            f.mirror.trigger(&unknown).await,
            Err(MirrorError::NotFound(_))
        ));
        assert!(asc_refs(&f).await.is_empty());
        // A primary repo is not a mirror.
        f.registry
            .create(&RepoName::parse("o/p").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let primary = RepoName::parse("o/p").unwrap();
        assert!(matches!(
            f.mirror.trigger(&primary).await,
            Err(MirrorError::NotMirror(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_syncs_both_succeed_with_one_set_of_rows() {
        let f = fixture().await;
        let c1 = push(&f.up, None, "a", "x", "refs/heads/master");
        push(&f.up, Some(&c1), "b", "y", "refs/heads/master");
        let name = RepoName::parse("o/m").unwrap();
        let (a, b) = tokio::join!(f.mirror.sync(&name), f.mirror.sync(&name));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.commits + b.commits, 2);
        assert_eq!(a.refs_set + b.refs_set, 1);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 1);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 1);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM cs_graph").await, 2);
    }

    #[tokio::test]
    async fn ids_equal_a_client_side_import_of_the_same_history() {
        let f = fixture().await;
        let c1 = push(&f.up, None, "a", "x", "refs/heads/master");
        push(&f.up, Some(&c1), "b", "y", "refs/heads/master");
        f.mirror
            .sync(&RepoName::parse("o/m").unwrap())
            .await
            .unwrap();

        let client = f
            .registry
            .create(&RepoName::parse("o/c").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let tips = f.mirror.list_refs(&f.up).await.unwrap();
        let odb = Odb::open(&f.up).unwrap();
        let map = SqlImportMap::new(f.registry.pool().clone(), &client);
        let report = Importer::new(
            client.backend.engine(),
            client.objects.clone(),
            &odb,
            &map,
            client.import,
        )
        .import_commits(&tips)
        .await
        .unwrap();
        let theirs = report.refs[0].1;
        let ours = f
            .registry
            .refs()
            .get(f.entry.key, &RefName::new("refs/heads/master").unwrap())
            .await
            .unwrap()
            .unwrap()
            .target;
        assert_eq!(ours, RefTarget::Changeset(theirs));
    }

    #[tokio::test]
    async fn unrepresentable_refs_are_skipped_and_a_bad_source_is_recorded() {
        let f = fixture().await;
        push(&f.up, None, "a", "x", "refs/heads/master");
        let tree = git(&f.up, &["mktree"], b"");
        git(&f.up, &["update-ref", "refs/tags/tree", &tree], b"");
        let name = RepoName::parse("o/m").unwrap();
        let report = f.mirror.sync(&name).await.unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("tree"));
        assert_eq!(asc_refs(&f).await.len(), 1);

        let missing = f._tmp.path().join("gone.git");
        f.mirror
            .sources()
            .set_configured(
                &name,
                MirrorTarget {
                    url: missing.to_str().unwrap().to_owned(),
                    token: None,
                },
            )
            .unwrap();
        assert!(matches!(
            f.mirror.sync(&name).await,
            Err(MirrorError::Git(_))
        ));
        let failed = "SELECT COUNT(*) FROM mirror_state WHERE last_error IS NOT NULL";
        assert_eq!(count(&f, failed).await, 1);
        // The refs are untouched by the failure.
        assert_eq!(asc_refs(&f).await.len(), 1);
    }
}
