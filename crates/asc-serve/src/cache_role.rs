//! The cache role, `asc serve --role cache --upstream <url>` (SPEC §7 "Cache role"; PLAN U70).
//!
//! A cache server has no SQL and no store. It serves `Version`, `HasObjects` and `GetObjects`
//! from a memory LRU and a disk LRU of *stored bytes* (`codec || data`, SPEC §3.2) and fetches
//! misses from the upstream server. Every other RPC answers `UNIMPLEMENTED`.
//!
//! * **Authorisation** is the upstream's. [`UpstreamAuthorizer`] asks the upstream (a one-key
//!   `HasObjects`, which needs read on the repository) and caches the answer per
//!   `(sha256(token), repo)` for 60 s when positive and 5 s when negative (SPEC §15.1). An
//!   upstream that cannot answer refuses the caller and caches nothing.
//! * **Namespaces** are kept apart: every cache entry is keyed by `(repo, key)`, so a caller who
//!   may read one repository never gets another repository's object through a shared key.
//! * **Presence is confirmed, never assumed.** A repository name is not a stable identity: the
//!   repository can be deleted and recreated, renamed or transferred under the same name, and the
//!   upstream exposes no repository id or generation. A cached object is therefore served only
//!   after the upstream has confirmed, for the calling principal and repository, that it still has
//!   the object, within the last [`POSITIVE_TTL`]. Confirmation is one batched `HasObjects` per
//!   request window; an object the upstream no longer has is dropped from both tiers and reported
//!   missing, and an upstream that cannot answer fails the request (fail closed). A fetch from the
//!   upstream confirms the objects it returns. The disk layout carries a version marker, and a
//!   directory written by an older layout is emptied at startup.
//! * **Integrity:** an object fetched from the upstream, or read back from disk, is decoded and
//!   hashed against its key before it is cached or served (a corrupt disk file is deleted and
//!   fetched again). Object sizes, window sizes and the disk budget are bounded.
//! * **Coalescing** (SPEC §16.3): concurrent misses on one `(repo, key)` share one upstream
//!   fetch; one request's misses go upstream in one `GetObjects` stream.
//! * **Restart:** the disk tier is indexed again at startup, so a restart loses nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::FileId;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::client::{CallError, ClientError, RpcClient};
use asc_rpc::coalesce::Coalesce;
use asc_rpc::server::wire as pb;
use asc_rpc::server::{
    CallCtx, MAX_RESPONSE_BYTES, ObjectsHandler, RepoName, RpcStream, ServerInfo, rpc_status,
};
use asc_store::key::ObjKey;
use asc_store::objects::decode_verified;
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{self, StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use tonic::transport::{Channel, Identity, Server, ServerTlsConfig};
use tonic::{Code, Status};

use crate::authz::{NEGATIVE_TTL, POSITIVE_TTL};
use crate::config::{CacheConfig, ConfigError, Role, ServeConfig};
use crate::http::Metrics;
use crate::registry::Extensions;
use crate::{ServeError, limits};

/// Keys resolved and held in memory at once per request window.
const WINDOW: usize = 64;
/// Windows resolved ahead of the one being sent.
const WINDOWS_AHEAD: usize = 2;
/// Cached `(token, repo)` authorisation answers.
const AUTH_ENTRIES: usize = 16_384;
/// Tokens kept to forward upstream (one per recently authorised principal).
const VAULT_ENTRIES: usize = 16_384;
/// Stored values under this size are verified inline instead of on a blocking thread.
const INLINE_VERIFY: usize = 32 * 1024;
/// Fixed per-entry overhead charged against the memory budget.
const ENTRY_OVERHEAD: u64 = 96;
/// A disk entry may use at most this fraction (1/n) of the disk budget.
const DISK_ENTRY_FRACTION: u64 = 4;
/// Length of the hex directory name that stands for a repository on disk.
const REPO_DIR_LEN: usize = 32;
/// Directory (under the disk root) where writes are staged.
const TMP_DIR: &str = "tmp";
/// File (under the disk root) that names the layout of the entries below it.
const LAYOUT_FILE: &str = "LAYOUT";
/// The current disk layout. Entries written under any other layout are not trusted: they were
/// cached without presence confirmation. Bump it when the meaning of an entry changes.
const LAYOUT: &str = "asc-cache-role-disk 2\n";
/// Domain separator of the repository directory name, so a directory from an older layout never
/// has the name the current layout looks up.
const REPO_DIR_DOMAIN: &[u8] = b"asc-cache-role/v2\0";
/// Presence confirmations remembered at once.
const CONFIRM_ENTRIES: usize = 131_072;

/// Why the cache role could not start.
#[derive(Debug, thiserror::Error)]
pub enum CacheRoleError {
    #[error("upstream: {0}")]
    Upstream(#[from] ClientError),
    #[error("disk cache {}: {source}", path.display())]
    Disk { path: PathBuf, source: io::Error },
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn digest_of(token: Option<&BearerToken>) -> [u8; 32] {
    Sha256::digest(token.map_or("", BearerToken::expose).as_bytes()).into()
}

// ------------------------------------------------------------------------------ upstream

/// One shared, lazily connected channel to the upstream server.
pub struct Upstream {
    channel: Channel,
}

impl Upstream {
    /// The upstream is https; plain http to the local machine needs the transport policy's
    /// opt-in (U89), because it would send tokens in the clear.
    pub fn connect(url: &str) -> Result<Self, ClientError> {
        Ok(Self {
            channel: asc_rpc::client::connect_channel(url)?,
        })
    }

    /// A client for `repo` that sends `token`.
    fn client(&self, repo: &str, token: Option<BearerToken>) -> Result<RpcClient, ClientError> {
        RpcClient::from_channel(self.channel.clone(), repo, token)
    }
}

/// An upstream failure as the status this server answers with. An upstream that misbehaves is
/// `UNAVAILABLE`, so a client moves on to its next tier.
fn call_status(e: CallError) -> Status {
    match e {
        CallError::Status { code, message } => Status::new(code, message),
        CallError::Rpc(e) => rpc_status(&e),
        CallError::Protocol(_) | CallError::Invalid(_) => {
            tracing::warn!(error = %e, "cache role: bad upstream answer");
            Status::unavailable("upstream sent an invalid answer")
        }
    }
}

// ------------------------------------------------------------------------------ tokens

/// The bearer tokens of recently authorised principals, so a handler can forward the caller's own
/// credential upstream. `CallCtx` carries the principal, not the token.
#[derive(Default)]
struct Vault(Option<quick_cache::sync::Cache<String, Option<BearerToken>>>);

impl Vault {
    fn new() -> Self {
        Self(Some(quick_cache::sync::Cache::new(VAULT_ENTRIES)))
    }

    fn put(&self, id: &str, token: Option<&BearerToken>) {
        if let Some(c) = &self.0 {
            c.insert(id.to_owned(), token.cloned());
        }
    }

    fn get(&self, id: &str) -> Option<Option<BearerToken>> {
        self.0.as_ref().and_then(|c| c.get(id))
    }
}

// ------------------------------------------------------------------------------ authorisation

type AuthKey = ([u8; 32], String);

#[derive(Clone)]
struct Verdict {
    at: Instant,
    answer: Result<(), AuthError>,
}

/// A refusal and whether it is definite enough to cache.
#[derive(Clone, Copy)]
struct Deny {
    err: AuthError,
    cache: bool,
}

/// Authorises by asking the upstream server (SPEC §7, §15.1).
pub struct UpstreamAuthorizer {
    upstream: Arc<Upstream>,
    cache: quick_cache::sync::Cache<AuthKey, Verdict>,
    flights: Coalesce<AuthKey, Result<(), Deny>>,
    vault: Arc<Vault>,
}

impl UpstreamAuthorizer {
    fn new(upstream: Arc<Upstream>, vault: Arc<Vault>) -> Self {
        Self {
            upstream,
            cache: quick_cache::sync::Cache::new(AUTH_ENTRIES),
            flights: Coalesce::new(),
            vault,
        }
    }
}

/// Read access on the repository is what a one-key `HasObjects` needs upstream.
async fn probe(client: RpcClient) -> Result<(), Deny> {
    let key = ObjKey::File(FileId::of(b""));
    match client.has_objects(&[key]).await {
        Ok(_) => Ok(()),
        Err(CallError::Status {
            code: Code::Unauthenticated,
            ..
        }) => Err(Deny {
            err: AuthError::Unauthenticated,
            cache: true,
        }),
        Err(CallError::Status {
            code: Code::PermissionDenied | Code::NotFound,
            ..
        }) => Err(Deny {
            err: AuthError::Forbidden,
            cache: true,
        }),
        Err(e) => {
            tracing::warn!(error = %e, "cache role: upstream authorisation lookup failed");
            Err(Deny {
                err: AuthError::Forbidden,
                cache: false,
            })
        }
    }
}

#[async_trait]
impl Authorizer for UpstreamAuthorizer {
    /// Every RPC is checked for read access: the cache serves nothing else, and a write RPC then
    /// reaches its handler and answers `UNIMPLEMENTED`.
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        _access: Access,
    ) -> Result<Principal, AuthError> {
        let name = RepoName::parse(repo).map_err(|_| AuthError::Forbidden)?;
        let digest = digest_of(token);
        let key: AuthKey = (digest, name.as_str().to_owned());
        let id = if token.is_some() {
            format!("token:{}", hex(&digest[..16]))
        } else {
            "anonymous".to_owned()
        };
        let cached = self.cache.get(&key).filter(|v| {
            v.at.elapsed()
                < if v.answer.is_ok() {
                    POSITIVE_TTL
                } else {
                    NEGATIVE_TTL
                }
        });
        let answer = if let Some(v) = cached {
            v.answer
        } else {
            let client = self
                .upstream
                .client(name.as_str(), token.cloned())
                .map_err(|_| AuthError::Forbidden)?;
            let got = self.flights.run(key.clone(), || probe(client)).await;
            let verdict = match got {
                Ok(()) => Some(Ok(())),
                Err(d) if d.cache => Some(Err(d.err)),
                Err(_) => None,
            };
            if let Some(answer) = verdict {
                self.cache.insert(
                    key,
                    Verdict {
                        at: Instant::now(),
                        answer,
                    },
                );
            }
            got.map_err(|d| d.err)
        };
        answer?;
        self.vault.put(&id, token);
        Ok(Principal { id })
    }
}

// ------------------------------------------------------------------------------ disk tier

struct DiskState {
    /// Relative path to `(tick, size)`.
    entries: HashMap<String, (u64, u64)>,
    /// Tick to relative path: the first entry is the least recently used.
    order: BTreeMap<u64, String>,
    total: u64,
    tick: u64,
}

impl DiskState {
    fn touch(&mut self, rel: &str) -> bool {
        let Some((tick, _)) = self.entries.get_mut(rel) else {
            return false;
        };
        self.order.remove(tick);
        self.tick += 1;
        *tick = self.tick;
        self.order.insert(self.tick, rel.to_owned());
        true
    }

    fn insert(&mut self, rel: String, size: u64) {
        self.remove(&rel);
        self.tick += 1;
        self.entries.insert(rel.clone(), (self.tick, size));
        self.order.insert(self.tick, rel);
        self.total += size;
    }

    fn remove(&mut self, rel: &str) -> bool {
        match self.entries.remove(rel) {
            Some((tick, size)) => {
                self.order.remove(&tick);
                self.total -= size;
                true
            }
            None => false,
        }
    }

    /// Entries to delete so that the total fits `max`, least recently used first.
    fn victims(&mut self, max: u64) -> Vec<String> {
        let mut out = Vec::new();
        while self.total > max {
            let Some((_, rel)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, size)) = self.entries.remove(&rel) {
                self.total -= size;
            }
            out.push(rel);
        }
        out
    }
}

/// Stored bytes as files under `<root>/<sha256(repo)[..32]>/<key path>`, bounded to `max` bytes
/// with least-recently-used eviction. Writes stage under `<root>/tmp` and rename into place, and a
/// symlink is never followed or read.
pub struct DiskCache {
    root: PathBuf,
    max: u64,
    state: Mutex<DiskState>,
}

/// A relative path this cache wrote: a repository directory and a verifiable object key.
fn valid_rel(rel: &str) -> bool {
    let Some((repo, rest)) = rel.split_once('/') else {
        return false;
    };
    repo.len() == REPO_DIR_LEN
        && repo
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !rest.starts_with("d/")
        && ObjKey::parse(rest).is_some()
}

impl DiskCache {
    /// Opens (creating) `root` and indexes the entries already there, oldest first. Blocking.
    pub fn open(root: &Path, max: u64) -> io::Result<Arc<Self>> {
        match std::fs::symlink_metadata(root) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(io::Error::other("the cache directory is a symlink"));
            }
            Ok(m) if !m.is_dir() => {
                return Err(io::Error::other("the cache directory is not a directory"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(root)?,
            Err(e) => return Err(e),
        }
        reset_foreign_layout(root)?;
        // Staged writes of a previous run are garbage.
        let tmp = root.join(TMP_DIR);
        if std::fs::symlink_metadata(&tmp).is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        std::fs::create_dir_all(&tmp)?;
        let mut found = Vec::new();
        for repo in std::fs::read_dir(root)? {
            let repo = repo?;
            let name = repo.file_name().to_string_lossy().into_owned();
            if repo.file_type()?.is_dir() && name.len() == REPO_DIR_LEN {
                scan(&repo.path(), &name, 0, &mut found);
            }
        }
        found.sort_by_key(|(_, _, modified)| *modified);
        let mut state = DiskState {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            total: 0,
            tick: 0,
        };
        for (rel, size, _) in found {
            state.insert(rel, size);
        }
        let this = Arc::new(Self {
            root: root.to_owned(),
            max,
            state: Mutex::new(state),
        });
        let victims = this.lock().victims(max);
        for rel in victims {
            let _ = std::fs::remove_file(this.root.join(&rel));
        }
        Ok(this)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DiskState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn rel(repo: &str, key: &ObjKey) -> String {
        let dir = hex(&Sha256::new()
            .chain_update(REPO_DIR_DOMAIN)
            .chain_update(repo.as_bytes())
            .finalize());
        format!("{}/{}", &dir[..REPO_DIR_LEN], key.path())
    }

    /// Bytes in use.
    pub fn total(&self) -> u64 {
        self.lock().total
    }

    async fn get(&self, rel: &str) -> Option<Bytes> {
        if !self.lock().touch(rel) {
            return None;
        }
        let path = self.root.join(rel);
        let read = async {
            let meta = tokio::fs::symlink_metadata(&path).await?;
            if !meta.is_file() || meta.len() > MAX_RESPONSE_BYTES as u64 {
                return Err(io::Error::other("not a regular file"));
            }
            tokio::fs::read(&path).await
        };
        match read.await {
            Ok(b) => Some(Bytes::from(b)),
            Err(_) => {
                self.lock().remove(rel);
                None
            }
        }
    }

    async fn remove(&self, rel: &str) {
        self.lock().remove(rel);
        let _ = tokio::fs::remove_file(self.root.join(rel)).await;
    }

    /// Deletes the entry when it is indexed. No file access otherwise.
    async fn evict(&self, rel: &str) {
        if self.lock().remove(rel) {
            let _ = tokio::fs::remove_file(self.root.join(rel)).await;
        }
    }

    async fn put(&self, rel: &str, data: &Bytes) {
        let size = data.len() as u64;
        if size == 0 || size > self.max / DISK_ENTRY_FRACTION {
            return;
        }
        if self.lock().touch(rel) {
            return;
        }
        let path = self.root.join(rel);
        let staged = self
            .root
            .join(TMP_DIR)
            .join(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
        let write = async {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&staged, data).await?;
            tokio::fs::rename(&staged, &path).await
        };
        if let Err(e) = write.await {
            let _ = tokio::fs::remove_file(&staged).await;
            // A concurrent writer of the same content wins the rename; that is fine.
            if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                tracing::warn!(error = %e, "cache role: disk write failed");
                return;
            }
        }
        let victims = {
            let mut s = self.lock();
            s.insert(rel.to_owned(), size);
            s.victims(self.max)
        };
        for v in victims {
            let _ = tokio::fs::remove_file(self.root.join(v)).await;
        }
    }
}

/// Empties `root` of entries unless its layout marker names the current layout, then writes the
/// marker. Only repository-named directories are removed; a symlink is never followed. Blocking.
fn reset_foreign_layout(root: &Path) -> io::Result<()> {
    let marker = root.join(LAYOUT_FILE);
    if std::fs::read_to_string(&marker).is_ok_and(|m| m == LAYOUT) {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && name.len() == REPO_DIR_LEN {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    let staged = root.join(format!("{LAYOUT_FILE}.tmp"));
    std::fs::write(&staged, LAYOUT)?;
    std::fs::rename(&staged, &marker)
}

/// Collects the regular files below one repository directory. Symlinks and unknown names are
/// ignored, never followed.
fn scan(dir: &Path, rel: &str, depth: usize, out: &mut Vec<(String, u64, SystemTime)>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = format!("{rel}/{name}");
        if ft.is_dir() && depth < 3 {
            scan(&entry.path(), &child, depth + 1, out);
        } else if ft.is_file()
            && valid_rel(&child)
            && let Ok(meta) = entry.metadata()
            && meta.len() > 0
            && meta.len() <= MAX_RESPONSE_BYTES as u64
        {
            out.push((
                child,
                meta.len(),
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ));
        }
    }
}

// ------------------------------------------------------------------------------ objects

type MemKey = (Arc<str>, ObjKey);
/// `(principal id, repo, key)`: who the upstream confirmed the object for.
type ConfirmKey = (Arc<str>, Arc<str>, ObjKey);

#[derive(Clone)]
struct ByteWeighter;

impl quick_cache::Weighter<MemKey, Bytes> for ByteWeighter {
    fn weight(&self, _key: &MemKey, val: &Bytes) -> u64 {
        val.len() as u64 + ENTRY_OVERHEAD
    }
}

type Resolved = Result<Option<Bytes>, Status>;
type Batch =
    futures::future::Shared<BoxFuture<'static, Result<Arc<HashMap<ObjKey, Bytes>>, Status>>>;

/// Decodes and hashes `stored` against `key`.
async fn verified(key: ObjKey, stored: &Bytes) -> bool {
    if stored.len() < INLINE_VERIFY {
        return decode_verified(&key, stored).is_ok();
    }
    let stored = stored.clone();
    tokio::task::spawn_blocking(move || decode_verified(&key, &stored).is_ok())
        .await
        .unwrap_or(false)
}

struct Inner {
    upstream: Arc<Upstream>,
    vault: Arc<Vault>,
    mem: Option<quick_cache::sync::Cache<MemKey, Bytes, ByteWeighter>>,
    disk: Option<Arc<DiskCache>>,
    flights: Coalesce<MemKey, Resolved>,
    metrics: Arc<Metrics>,
    /// When the upstream last confirmed that a repository has an object, per principal.
    confirmed: quick_cache::sync::Cache<ConfirmKey, Instant>,
    /// How long a confirmation lets cached bytes be served without asking the upstream again.
    confirm_ttl: Duration,
}

impl Inner {
    fn count(&self, result: &str, n: u64) {
        if n > 0 {
            self.metrics
                .cache_requests
                .get_or_create(&vec![
                    ("tier".to_owned(), "cache_role".to_owned()),
                    ("result".to_owned(), result.to_owned()),
                ])
                .inc_by(n);
        }
    }

    fn token_of(&self, ctx: &CallCtx) -> Result<Option<BearerToken>, Status> {
        self.vault
            .get(&ctx.principal.id)
            .ok_or_else(|| Status::unavailable("credentials expired; retry"))
    }

    fn is_confirmed(&self, who: &Arc<str>, repo: &Arc<str>, key: ObjKey) -> bool {
        self.confirmed
            .get(&(who.clone(), repo.clone(), key))
            .is_some_and(|at| at.elapsed() < self.confirm_ttl)
    }

    fn confirm(&self, who: &Arc<str>, repo: &Arc<str>, key: ObjKey) {
        self.confirmed
            .insert((who.clone(), repo.clone(), key), Instant::now());
    }

    /// Drops the cached copy of an object the upstream no longer has for this repository name.
    async fn purge(&self, repo: &Arc<str>, key: ObjKey) {
        if let Some(mem) = &self.mem {
            mem.remove(&(repo.clone(), key));
        }
        if let Some(disk) = &self.disk {
            disk.evict(&DiskCache::rel(repo, &key)).await;
        }
    }

    /// Asks the upstream, with the caller's credential, which of `keys` the repository has.
    async fn upstream_has(
        &self,
        repo: &Arc<str>,
        token: Option<BearerToken>,
        keys: &[ObjKey],
    ) -> Result<Vec<bool>, Status> {
        let client = self
            .upstream
            .client(repo, token)
            .map_err(|_| Status::unavailable("upstream unavailable"))?;
        let have = client.has_objects(keys).await.map_err(call_status)?;
        if have.len() == keys.len() {
            Ok(have)
        } else {
            tracing::warn!("cache role: upstream presence answer has the wrong length");
            Err(Status::unavailable("upstream sent an invalid answer"))
        }
    }

    fn mem_get(&self, repo: &Arc<str>, key: ObjKey) -> Option<Bytes> {
        self.mem.as_ref()?.get(&(repo.clone(), key))
    }

    async fn store(&self, repo: &Arc<str>, key: ObjKey, stored: Bytes) {
        if let Some(mem) = &self.mem {
            mem.insert((repo.clone(), key), stored.clone());
        }
        if let Some(disk) = &self.disk {
            disk.put(&DiskCache::rel(repo, &key), &stored).await;
        }
    }

    /// One `GetObjects` stream upstream for `keys` (distinct), verified.
    async fn fetch_batch(
        self: Arc<Self>,
        repo: Arc<str>,
        token: Option<BearerToken>,
        keys: Vec<ObjKey>,
    ) -> Result<Arc<HashMap<ObjKey, Bytes>>, Status> {
        let client = self
            .upstream
            .client(&repo, token)
            .map_err(|_| Status::unavailable("upstream unavailable"))?;
        let mut got = HashMap::with_capacity(keys.len());
        let mut items = client.fetch(keys);
        while let Some(item) = items.next().await {
            let (key, stored) = item.map_err(call_status)?;
            if stored.len() > MAX_RESPONSE_BYTES {
                return Err(Status::resource_exhausted(
                    "object is larger than one response message",
                ));
            }
            if !verified(key, &stored).await {
                tracing::error!(%key, "cache role: upstream object does not match its key");
                return Err(Status::data_loss("upstream object does not match its key"));
            }
            got.insert(key, stored);
        }
        Ok(Arc::new(got))
    }

    /// The stored bytes of each of `keys` (`None` when the upstream has none), in order.
    async fn resolve(
        self: &Arc<Self>,
        who: &Arc<str>,
        repo: &Arc<str>,
        token: Option<BearerToken>,
        keys: &[ObjKey],
    ) -> Result<Vec<Option<Bytes>>, Status> {
        let mut found: Vec<Option<Bytes>> = vec![None; keys.len()];
        let mut need = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match self.mem_get(repo, *k) {
                Some(b) => found[i] = Some(b),
                None => need.push(i),
            }
        }
        self.count("memory_hit", (keys.len() - need.len()) as u64);
        if let (Some(disk), false) = (&self.disk, need.is_empty()) {
            let reads = futures::future::join_all(need.iter().map(|&i| {
                let rel = DiskCache::rel(repo, &keys[i]);
                async move {
                    let b = disk.get(&rel).await?;
                    if verified(keys[i], &b).await {
                        Some(b)
                    } else {
                        disk.remove(&rel).await;
                        None
                    }
                }
            }))
            .await;
            let mut still = Vec::new();
            let mut hits = 0;
            for (i, r) in need.into_iter().zip(reads) {
                match r {
                    Some(b) => {
                        hits += 1;
                        if let Some(mem) = &self.mem {
                            mem.insert((repo.clone(), keys[i]), b.clone());
                        }
                        found[i] = Some(b);
                    }
                    None => still.push(i),
                }
            }
            self.count("disk_hit", hits);
            need = still;
        }
        // Cached bytes are served only once the upstream confirms that the repository, as it is
        // now, still has them. One batched call covers the whole window.
        let unconfirmed: Vec<usize> = (0..keys.len())
            .filter(|&i| found[i].is_some() && !self.is_confirmed(who, repo, keys[i]))
            .collect();
        if !unconfirmed.is_empty() {
            let mut seen = HashSet::new();
            let asked: Vec<ObjKey> = unconfirmed
                .iter()
                .map(|&i| keys[i])
                .filter(|k| seen.insert(*k))
                .collect();
            let have = self.upstream_has(repo, token.clone(), &asked).await?;
            let present: HashMap<ObjKey, bool> = asked.into_iter().zip(have).collect();
            for (&k, &h) in &present {
                if h {
                    self.confirm(who, repo, k);
                } else {
                    self.purge(repo, k).await;
                }
            }
            for i in unconfirmed {
                if !present[&keys[i]] {
                    found[i] = None;
                }
            }
        }
        if need.is_empty() {
            return Ok(found);
        }
        let mut seen = HashSet::new();
        let misses: Vec<ObjKey> = need
            .iter()
            .map(|&i| keys[i])
            .filter(|k| seen.insert(*k))
            .collect();
        self.count("miss", misses.len() as u64);
        // The batch runs only if some flight below is new; a key somebody else already fetches
        // joins that flight instead.
        let batch: Batch = self
            .clone()
            .fetch_batch(repo.clone(), token, misses.clone())
            .boxed()
            .shared();
        let fetched = futures::future::join_all(misses.iter().map(|&k| {
            let (batch, this, repo) = (batch.clone(), self.clone(), repo.clone());
            self.flights.run((repo.clone(), k), move || async move {
                let map = batch.await?;
                let stored = map.get(&k).cloned();
                if let Some(b) = &stored {
                    this.store(&repo, k, b.clone()).await;
                }
                Ok(stored)
            })
        }))
        .await;
        let mut by_key = HashMap::with_capacity(misses.len());
        for (k, r) in misses.into_iter().zip(fetched) {
            by_key.insert(k, r?);
        }
        for i in need {
            found[i].clone_from(&by_key[&keys[i]]);
            if found[i].is_some() {
                self.confirm(who, repo, keys[i]);
            }
        }
        Ok(found)
    }
}

/// The wire form of `key`.
fn wire_key(key: &ObjKey) -> Result<pb::ObjKey, Status> {
    pb::ObjKey::try_from(key).map_err(|e| Status::internal(format!("object key: {e}")))
}

/// Packs `items` into responses of at most [`MAX_RESPONSE_BYTES`].
fn pack(items: Vec<(ObjKey, Option<Bytes>)>) -> Result<Vec<pb::GetObjectsResponse>, Status> {
    let mut out = Vec::new();
    let mut cur = pb::GetObjectsResponse::default();
    let mut size = 0usize;
    for (key, stored) in items {
        match stored {
            None => cur.missing.push(wire_key(&key)?),
            Some(b) => {
                if b.len() > MAX_RESPONSE_BYTES {
                    return Err(Status::resource_exhausted(
                        "object is larger than one response message",
                    ));
                }
                if size + b.len() > MAX_RESPONSE_BYTES {
                    out.push(std::mem::take(&mut cur));
                    size = 0;
                }
                size += b.len();
                cur.objects.push(pb::Object {
                    key: Some(wire_key(&key)?),
                    data: b,
                });
            }
        }
    }
    if !cur.objects.is_empty() || !cur.missing.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// The tier handler: `HasObjects` and `GetObjects` over the memory and disk caches.
pub struct CacheObjects {
    inner: Arc<Inner>,
}

#[async_trait]
impl ObjectsHandler for CacheObjects {
    async fn has_objects(
        &self,
        ctx: &CallCtx,
        req: pb::HasObjectsRequest,
    ) -> Result<pb::HasObjectsResponse, Status> {
        let inner = &self.inner;
        let keys = req
            .keys
            .iter()
            .map(|k| ObjKey::try_from(k).map_err(|e| Status::invalid_argument(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let repo: Arc<str> = Arc::from(ctx.repo.as_str());
        let who: Arc<str> = Arc::from(ctx.principal.id.as_str());
        let mut present = vec![false; keys.len()];
        let mut unknown = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            // Holding an object is not proof that this repository has it (the name can be reused
            // by a different repository), so only a recent upstream confirmation counts.
            if inner.is_confirmed(&who, &repo, *k) {
                present[i] = true;
            } else {
                unknown.push(i);
            }
        }
        inner.count("presence_hit", (keys.len() - unknown.len()) as u64);
        if !unknown.is_empty() {
            let token = inner.token_of(ctx)?;
            let asked: Vec<ObjKey> = unknown.iter().map(|&i| keys[i]).collect();
            let have = inner.upstream_has(&repo, token, &asked).await?;
            for ((i, h), k) in unknown.into_iter().zip(have).zip(asked) {
                present[i] = h;
                if h {
                    inner.confirm(&who, &repo, k);
                } else {
                    inner.purge(&repo, k).await;
                }
            }
        }
        let mut bitmap = vec![0u8; keys.len().div_ceil(8)];
        for (i, _) in present.iter().enumerate().filter(|(_, p)| **p) {
            bitmap[i / 8] |= 1 << (i % 8);
        }
        Ok(pb::HasObjectsResponse {
            present: Bytes::from(bitmap),
        })
    }

    async fn get_objects(
        &self,
        ctx: &CallCtx,
        req: pb::GetObjectsRequest,
    ) -> Result<RpcStream<pb::GetObjectsResponse>, Status> {
        let keys = req
            .keys
            .iter()
            .map(|k| ObjKey::try_from(k).map_err(|e| Status::invalid_argument(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let repo: Arc<str> = Arc::from(ctx.repo.as_str());
        let who: Arc<str> = Arc::from(ctx.principal.id.as_str());
        let token = self.inner.token_of(ctx)?;
        let windows: Vec<Vec<ObjKey>> = keys.chunks(WINDOW).map(<[ObjKey]>::to_vec).collect();
        let inner = self.inner.clone();
        let out = stream::iter(windows)
            .map(move |window| {
                let (inner, who, repo, token) =
                    (inner.clone(), who.clone(), repo.clone(), token.clone());
                async move {
                    let got = inner.resolve(&who, &repo, token, &window).await?;
                    pack(window.into_iter().zip(got).collect())
                }
            })
            .buffered(WINDOWS_AHEAD)
            .map_ok(|responses| stream::iter(responses.into_iter().map(Ok)))
            .try_flatten()
            .boxed();
        Ok(out)
    }
}

// ------------------------------------------------------------------------------ the role

/// A cache server's parts: the authorizer and the handler for the tier RPCs.
pub struct CacheRole {
    authorizer: Arc<UpstreamAuthorizer>,
    objects: Arc<CacheObjects>,
}

impl CacheRole {
    /// Builds the role over `upstream_url`. Opens (and indexes) the disk tier. Blocking.
    pub fn new(
        upstream_url: &str,
        cache: &CacheConfig,
        metrics: Arc<Metrics>,
    ) -> Result<Self, CacheRoleError> {
        Self::build(upstream_url, cache, metrics, POSITIVE_TTL)
    }

    fn build(
        upstream_url: &str,
        cache: &CacheConfig,
        metrics: Arc<Metrics>,
        confirm_ttl: Duration,
    ) -> Result<Self, CacheRoleError> {
        let upstream = Arc::new(Upstream::connect(upstream_url)?);
        let vault = Arc::new(Vault::new());
        let mem = (cache.memory.0 > 0).then(|| {
            quick_cache::sync::Cache::with_weighter(
                usize::try_from(cache.memory.0 / (16 * 1024))
                    .unwrap_or(usize::MAX)
                    .clamp(1024, 1 << 22),
                cache.memory.0,
                ByteWeighter,
            )
        });
        let disk =
            match (&cache.disk_dir, cache.disk.0) {
                (Some(dir), max) if max > 0 => Some(DiskCache::open(dir, max).map_err(
                    |source| CacheRoleError::Disk {
                        path: dir.clone(),
                        source,
                    },
                )?),
                _ => None,
            };
        let authorizer = Arc::new(UpstreamAuthorizer::new(upstream.clone(), vault.clone()));
        let objects = Arc::new(CacheObjects {
            inner: Arc::new(Inner {
                upstream,
                vault,
                mem,
                disk,
                flights: Coalesce::new(),
                metrics,
                confirmed: quick_cache::sync::Cache::new(CONFIRM_ENTRIES),
                confirm_ttl,
            }),
        });
        Ok(Self {
            authorizer,
            objects,
        })
    }

    pub fn authorizer(&self) -> Arc<dyn Authorizer> {
        self.authorizer.clone()
    }

    /// The registrations of the cache role: the tier handler only; every other RPC group stays
    /// unset and answers `UNIMPLEMENTED`.
    pub fn extensions(&self) -> Extensions {
        let ext = Extensions::new();
        if ext.handlers.set_objects(self.objects.clone()).is_err() {
            tracing::error!("cache role: the objects handler was already registered");
        }
        ext.http
            .register(axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" })));
        ext
    }
}

/// The admin listener of a cache server: `/metrics`, behind the admin token when one is set
/// (Bearer or Basic, constant-time, with the Basic challenge first: U78's `authn::guard_admin`).
fn admin_router(metrics: Arc<Metrics>, token: Option<Arc<str>>) -> axum::Router {
    use axum::http::header;
    const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";
    let routes = axum::Router::new()
        .route(
            "/metrics",
            axum::routing::get(
                |axum::extract::State(m): axum::extract::State<Arc<Metrics>>| async move {
                    ([(header::CONTENT_TYPE, OPENMETRICS)], m.render())
                },
            ),
        )
        .with_state(metrics);
    crate::authn::guard_admin(routes, token)
}

/// Runs the cache server until `shutdown` resolves.
pub async fn serve(cfg: ServeConfig, shutdown: impl Future<Output = ()>) -> Result<(), ServeError> {
    cfg.validate()?;
    let invalid = |m: String| ServeError::Config(ConfigError::Invalid(m));
    if cfg.role != Role::Cache {
        return Err(ServeError::RoleUnavailable(cfg.role));
    }
    let upstream = cfg
        .upstream
        .as_deref()
        .ok_or_else(|| invalid("the cache role needs `upstream`".into()))?;
    let metrics = Arc::new(Metrics::new());
    let role = CacheRole::new(upstream, &cfg.cache, metrics.clone())
        .map_err(|e| invalid(e.to_string()))?;
    let ext = role.extensions();
    let admission = Arc::new(limits::Admission::new(&cfg.limits));
    let routes = limits::guarded_routes(&ext, role.authorizer(), admission, ServerInfo::default());

    let admin_token = match &cfg.admin.token_file {
        Some(p) => Some(crate::read_token(p)?),
        None => None,
    };
    let admin_listener = tokio::net::TcpListener::bind(cfg.admin.listen)
        .await
        .map_err(|e| ServeError::Admin(format!("{}: {e}", cfg.admin.listen)))?;
    let (admin_stop, admin_stopped) = tokio::sync::oneshot::channel::<()>();
    let admin_app = admin_router(metrics, admin_token);
    let admin = tokio::spawn(async move {
        axum::serve(admin_listener, admin_app)
            .with_graceful_shutdown(async {
                let _ = admin_stopped.await;
            })
            .await
    });
    tracing::info!(listen = %cfg.admin.listen, "asc serve (cache) admin listening");

    let mut server = Server::builder().accept_http1(true);
    if let Some(tls) = &cfg.tls {
        let identity = Identity::from_pem(crate::read_pem(&tls.cert)?, crate::read_pem(&tls.key)?);
        server = server.tls_config(ServerTlsConfig::new().identity(identity))?;
    }
    let mut server = server.layer(crate::alt_svc::layer(cfg.http3_port));
    // The same router over QUIC when `[tls]` and `http3_port` are set (U89b); held until return.
    let _h3 = crate::h3_listener::start(&cfg, &routes)?;
    tracing::info!(listen = %cfg.listen, "asc serve (cache) listening");
    let served = server
        .add_routes(routes)
        .serve_with_shutdown(cfg.listen, async move {
            shutdown.await;
            let _ = admin_stop.send(());
        })
        .await;
    admin.abort();
    served?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use asc_rpc::auth::StaticTokens;
    use asc_rpc::server::{RepoHandlers, RepoService};
    use asc_store::codec::encode_stored;
    use tokio::sync::oneshot;
    use tonic::transport::server::TcpIncoming;

    use super::*;
    use crate::config::ByteSize;

    const REPO: &str = "owner/repo";
    const TOKEN: &str = "tok";

    #[derive(Default)]
    struct Counters {
        gets: AtomicUsize,
        has: AtomicUsize,
    }

    /// An upstream tier with a fixed object set that counts its calls.
    struct Fake {
        objects: Arc<Mutex<HashMap<ObjKey, Bytes>>>,
        counters: Arc<Counters>,
        delay: Duration,
    }

    #[async_trait]
    impl ObjectsHandler for Fake {
        async fn has_objects(
            &self,
            _ctx: &CallCtx,
            req: pb::HasObjectsRequest,
        ) -> Result<pb::HasObjectsResponse, Status> {
            self.counters.has.fetch_add(1, Ordering::SeqCst);
            let mut bitmap = vec![0u8; req.keys.len().div_ceil(8)];
            for (i, k) in req.keys.iter().enumerate() {
                let k = ObjKey::try_from(k).unwrap();
                if self.objects.lock().unwrap().contains_key(&k) {
                    bitmap[i / 8] |= 1 << (i % 8);
                }
            }
            Ok(pb::HasObjectsResponse {
                present: Bytes::from(bitmap),
            })
        }

        async fn get_objects(
            &self,
            _ctx: &CallCtx,
            req: pb::GetObjectsRequest,
        ) -> Result<RpcStream<pb::GetObjectsResponse>, Status> {
            self.counters.gets.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            let mut resp = pb::GetObjectsResponse::default();
            for k in &req.keys {
                let key = ObjKey::try_from(k).unwrap();
                match self.objects.lock().unwrap().get(&key) {
                    Some(b) => resp.objects.push(pb::Object {
                        key: Some(k.clone()),
                        data: b.clone(),
                    }),
                    None => resp.missing.push(k.clone()),
                }
            }
            Ok(stream::once(async { Ok(resp) }).boxed())
        }
    }

    struct Running {
        url: String,
        _stop: oneshot::Sender<()>,
    }

    async fn listen() -> (TcpIncoming, SocketAddr) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        (TcpIncoming::from(l), addr)
    }

    /// A server that only knows `Fake` and accepts `TOKEN` for reading.
    async fn upstream(fake: Fake) -> Running {
        // The fixtures serve plain http on the loopback interface (U89 opt-in).
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        let handlers = Arc::new(RepoHandlers::new());
        handlers.set_objects(Arc::new(fake)).ok().unwrap();
        let auth = StaticTokens::new().with_token(TOKEN, "alice", Access::Read);
        let svc = RepoService::new(handlers, Arc::new(auth), ServerInfo::default()).into_service();
        let (incoming, addr) = listen().await;
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(svc)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = rx.await;
                })
                .await;
        });
        Running {
            url: format!("http://{addr}"),
            _stop: tx,
        }
    }

    fn settings(dir: Option<&Path>) -> CacheConfig {
        CacheConfig {
            memory: ByteSize(64 * ByteSize::MIB),
            disk_dir: dir.map(Path::to_owned),
            disk: ByteSize(if dir.is_some() { 64 * ByteSize::MIB } else { 0 }),
        }
    }

    async fn cache(upstream_url: &str, dir: Option<&Path>) -> (Running, CacheRole) {
        cache_ttl(upstream_url, dir, POSITIVE_TTL).await
    }

    /// A cache server that trusts an upstream confirmation for `confirm_ttl`.
    async fn cache_ttl(
        upstream_url: &str,
        dir: Option<&Path>,
        confirm_ttl: Duration,
    ) -> (Running, CacheRole) {
        let role = CacheRole::build(
            upstream_url,
            &settings(dir),
            Arc::new(Metrics::new()),
            confirm_ttl,
        )
        .unwrap();
        let ext = role.extensions();
        let routes = crate::build_routes(&ext, role.authorizer(), ServerInfo::default());
        let (incoming, addr) = listen().await;
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_routes(routes)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = rx.await;
                })
                .await;
        });
        (
            Running {
                url: format!("http://{addr}"),
                _stop: tx,
            },
            role,
        )
    }

    fn object(data: &[u8]) -> (ObjKey, Bytes) {
        (ObjKey::File(FileId::of(data)), encode_stored(data))
    }

    fn token() -> Option<BearerToken> {
        Some(BearerToken::new(TOKEN).unwrap())
    }

    type Objects = Arc<Mutex<HashMap<ObjKey, Bytes>>>;

    fn fake(objects: &[(ObjKey, Bytes)], delay: Duration) -> (Fake, Arc<Counters>) {
        let (f, counters, _) = fake_mut(objects, delay);
        (f, counters)
    }

    /// Like [`fake`], with a handle that changes the upstream's object set afterwards.
    fn fake_mut(objects: &[(ObjKey, Bytes)], delay: Duration) -> (Fake, Arc<Counters>, Objects) {
        let counters = Arc::new(Counters::default());
        let set: Objects = Arc::new(Mutex::new(objects.iter().cloned().collect()));
        (
            Fake {
                objects: set.clone(),
                counters: counters.clone(),
                delay,
            },
            counters,
            set,
        )
    }

    async fn fetch_all(
        client: &RpcClient,
        keys: &[ObjKey],
    ) -> Result<Vec<(ObjKey, Bytes)>, CallError> {
        client.fetch(keys.to_vec()).try_collect().await
    }

    #[tokio::test]
    async fn an_objects_tier_client_never_falls_back_to_the_primary() {
        let objs: Vec<_> = (0..5u8).map(|i| object(&[i; 100])).collect();
        let (f, up_counters) = fake(&objs, Duration::ZERO);
        let up = upstream(f).await;
        let (trip_fake, tripwire) = fake(&objs, Duration::ZERO);
        let primary = upstream(trip_fake).await;
        let (cache, _role) = cache(&up.url, None).await;

        let client = RpcClient::connect(&primary.url, REPO, token())
            .unwrap()
            .with_object_tiers(&[&cache.url])
            .unwrap();
        let keys: Vec<ObjKey> = objs.iter().map(|(k, _)| *k).collect();
        let mut got = fetch_all(&client, &keys).await.unwrap();
        got.sort_by_key(|(k, _)| k.path());
        let mut want = objs.clone();
        want.sort_by_key(|(k, _)| k.path());
        assert_eq!(got, want);
        assert_eq!(client.has_objects(&keys).await.unwrap(), vec![true; 5]);
        let absent = object(b"absent").0;
        assert_eq!(client.has_objects(&[absent]).await.unwrap(), vec![false]);
        // A second read is served from memory.
        fetch_all(&client, &keys).await.unwrap();
        assert_eq!(up_counters.gets.load(Ordering::SeqCst), 1);
        assert_eq!(tripwire.gets.load(Ordering::SeqCst), 0);
        assert_eq!(tripwire.has.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_missing_object_is_reported_missing() {
        let (f, _) = fake(&[], Duration::ZERO);
        let up = upstream(f).await;
        let (cache, _role) = cache(&up.url, None).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        let got = fetch_all(&client, &[object(b"nope").0]).await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn a_restart_loses_nothing() {
        let objs: Vec<_> = (0..4u8).map(|i| object(&[i; 2000])).collect();
        let keys: Vec<ObjKey> = objs.iter().map(|(k, _)| *k).collect();
        let (f, counters) = fake(&objs, Duration::ZERO);
        let up = upstream(f).await;
        let dir = tempfile::tempdir().unwrap();
        {
            let (cache, role) = cache(&up.url, Some(dir.path())).await;
            let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
            assert_eq!(fetch_all(&client, &keys).await.unwrap().len(), 4);
            assert_eq!(counters.gets.load(Ordering::SeqCst), 1);
            drop((cache, role));
        }
        let (cache, _role) = cache(&up.url, Some(dir.path())).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        let mut got = fetch_all(&client, &keys).await.unwrap();
        got.sort_by_key(|(k, _)| k.path());
        let mut want = objs.clone();
        want.sort_by_key(|(k, _)| k.path());
        assert_eq!(got, want);
        assert_eq!(counters.gets.load(Ordering::SeqCst), 1, "served from disk");
    }

    #[tokio::test]
    async fn fifty_concurrent_misses_make_one_upstream_call() {
        let obj = object(&[7u8; 4096]);
        let (f, counters) = fake(std::slice::from_ref(&obj), Duration::from_millis(400));
        let up = upstream(f).await;
        let (cache, _role) = cache(&up.url, None).await;
        let mut tasks = Vec::new();
        for _ in 0..50 {
            let url = cache.url.clone();
            let (key, stored) = obj.clone();
            tasks.push(tokio::spawn(async move {
                let client = RpcClient::connect(&url, REPO, token()).unwrap();
                let got = fetch_all(&client, &[key]).await.unwrap();
                assert_eq!(got, vec![(key, stored)]);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(counters.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn other_rpcs_are_unimplemented() {
        let (f, _) = fake(&[], Duration::ZERO);
        let up = upstream(f).await;
        let (cache, _role) = cache(&up.url, None).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        match client.list_refs_all().await.unwrap_err() {
            CallError::Status { code, .. } => assert_eq!(code, Code::Unimplemented),
            e => panic!("{e}"),
        }
        client.server_version().await.unwrap();
        let (k, b) = object(b"x");
        match client.put_objects(vec![(k, b)]).await.unwrap_err() {
            CallError::Status { code, .. } => assert_eq!(code, Code::Unimplemented),
            e => panic!("{e}"),
        }
    }

    #[tokio::test]
    async fn the_upstream_decides_who_may_read() {
        let obj = object(b"secret");
        let (f, counters) = fake(std::slice::from_ref(&obj), Duration::ZERO);
        let up = upstream(f).await;
        let (cache, _role) = cache(&up.url, None).await;
        let good = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        assert_eq!(fetch_all(&good, &[obj.0]).await.unwrap().len(), 1);
        for bad in [None, Some(BearerToken::new("wrong").unwrap())] {
            let client = RpcClient::connect(&cache.url, REPO, bad).unwrap();
            assert!(fetch_all(&client, &[obj.0]).await.is_err());
            assert!(client.has_objects(&[obj.0]).await.is_err());
        }
        assert_eq!(counters.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn authorisation_answers_are_cached_per_token_and_repo() {
        let (f, counters) = fake(&[], Duration::ZERO);
        let up = upstream(f).await;
        let (_cache, role) = cache(&up.url, None).await;
        let auth = role.authorizer();
        let t = BearerToken::new(TOKEN).unwrap();
        for _ in 0..3 {
            auth.authorize(Some(&t), REPO, Access::Read).await.unwrap();
        }
        assert_eq!(counters.has.load(Ordering::SeqCst), 1);
        auth.authorize(Some(&t), "owner/other", Access::Read)
            .await
            .unwrap();
        assert_eq!(counters.has.load(Ordering::SeqCst), 2);
        let bad = BearerToken::new("wrong").unwrap();
        for _ in 0..3 {
            assert_eq!(
                auth.authorize(Some(&bad), REPO, Access::Read)
                    .await
                    .unwrap_err(),
                AuthError::Unauthenticated
            );
        }
        assert_eq!(
            auth.authorize(Some(&t), "../escape", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
    }

    #[tokio::test]
    async fn an_upstream_object_that_fails_its_hash_is_refused_and_not_cached() {
        let (key, _) = object(b"honest");
        let (_, lie) = object(b"forged");
        let (f, _) = fake(&[(key, lie)], Duration::ZERO);
        let up = upstream(f).await;
        let dir = tempfile::tempdir().unwrap();
        let (cache, _role) = cache(&up.url, Some(dir.path())).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        assert!(fetch_all(&client, &[key]).await.is_err());
        assert_eq!(client.has_objects(&[key]).await.unwrap(), vec![true]);
        let cached = walk_files(dir.path());
        assert!(cached.is_empty(), "{cached:?}");
    }

    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                out.extend(walk_files(&p));
            } else if e.file_name() != LAYOUT_FILE {
                out.push(p);
            }
        }
        out
    }

    #[tokio::test]
    async fn a_corrupt_disk_file_is_dropped_and_fetched_again() {
        let obj = object(&[9u8; 3000]);
        let (f, counters) = fake(std::slice::from_ref(&obj), Duration::ZERO);
        let up = upstream(f).await;
        let dir = tempfile::tempdir().unwrap();
        {
            let (cache, _role) = cache(&up.url, Some(dir.path())).await;
            let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
            fetch_all(&client, &[obj.0]).await.unwrap();
        }
        let files = walk_files(dir.path());
        assert_eq!(files.len(), 1, "{files:?}");
        let mut bad = std::fs::read(&files[0]).unwrap();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        std::fs::write(&files[0], bad).unwrap();

        let (cache, _role) = cache(&up.url, Some(dir.path())).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        let got = fetch_all(&client, &[obj.0]).await.unwrap();
        assert_eq!(got, vec![obj.clone()]);
        assert_eq!(counters.gets.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&files[0]).unwrap(), obj.1.to_vec());
    }

    #[tokio::test]
    async fn the_disk_tier_evicts_the_least_recently_used_entry() {
        // Incompressible data keeps every stored value near its raw size.
        let noise = |i: u8, n: usize| -> Vec<u8> {
            let mut out = vec![0u8; n];
            blake3::Hasher::new()
                .update(&[i])
                .finalize_xof()
                .fill(&mut out);
            out
        };
        let dir = tempfile::tempdir().unwrap();
        let disk = DiskCache::open(dir.path(), 5000).unwrap();
        let objs: Vec<_> = (0..5u8).map(|i| object(&noise(i, 1000))).collect();
        let rels: Vec<String> = objs.iter().map(|(k, _)| DiskCache::rel(REPO, k)).collect();
        for i in 0..4 {
            disk.put(&rels[i], &objs[i].1).await;
        }
        assert!(disk.total() > 4000 && disk.total() <= 5000);
        // Reading the first makes the second the oldest.
        assert!(disk.get(&rels[0]).await.is_some());
        disk.put(&rels[4], &objs[4].1).await;
        assert!(disk.total() <= 5000);
        assert!(disk.get(&rels[0]).await.is_some());
        assert!(disk.get(&rels[1]).await.is_none());
        assert!(disk.get(&rels[4]).await.is_some());
        // An entry above the per-entry share is not stored.
        let big = object(&noise(9, 2000));
        disk.put(&DiskCache::rel(REPO, &big.0), &big.1).await;
        assert!(disk.get(&DiskCache::rel(REPO, &big.0)).await.is_none());
    }

    #[tokio::test]
    async fn namespaces_do_not_share_cache_entries() {
        let obj = object(b"shared bytes");
        let (f, counters) = fake(std::slice::from_ref(&obj), Duration::ZERO);
        let up = upstream(f).await;
        let (cache, _role) = cache(&up.url, None).await;
        for repo in ["owner/a", "owner/b"] {
            let client = RpcClient::connect(&cache.url, repo, token()).unwrap();
            assert_eq!(fetch_all(&client, &[obj.0]).await.unwrap().len(), 1);
        }
        assert_eq!(counters.gets.load(Ordering::SeqCst), 2);
    }

    /// The upstream repository behind a name is replaced by one without `obj`.
    fn recreate(set: &Objects) {
        set.lock().unwrap().clear();
    }

    #[tokio::test]
    async fn a_recreated_repository_is_not_served_the_old_ones_objects() {
        let obj = object(&[5u8; 3000]);
        let keep = object(&[6u8; 3000]);
        let (f, _, set) = fake_mut(&[obj.clone(), keep.clone()], Duration::ZERO);
        let up = upstream(f).await;
        let dir = tempfile::tempdir().unwrap();
        // Every request confirms with the upstream: the recreation happens between two requests.
        let (cache, _role) = cache_ttl(&up.url, Some(dir.path()), Duration::ZERO).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        let keys = [obj.0, keep.0];
        assert_eq!(fetch_all(&client, &keys).await.unwrap().len(), 2);
        assert_eq!(client.has_objects(&keys).await.unwrap(), vec![true, true]);
        assert_eq!(walk_files(dir.path()).len(), 2);

        // Same name, new repository: it has `keep` again, but not `obj`.
        recreate(&set);
        set.lock().unwrap().insert(keep.0, keep.1.clone());
        assert_eq!(client.has_objects(&keys).await.unwrap(), vec![false, true]);
        let got = fetch_all(&client, &keys).await.unwrap();
        assert_eq!(
            got,
            vec![keep.clone()],
            "the old repository's object leaked"
        );
        let left = walk_files(dir.path());
        assert_eq!(left.len(), 1, "the stale disk copy is dropped: {left:?}");

        // A restart serves nothing from the disk tier that the upstream no longer has.
        recreate(&set);
        let (cache, _role) = cache_ttl(&up.url, Some(dir.path()), Duration::ZERO).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        assert!(fetch_all(&client, &keys).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cached_object_is_confirmed_once_per_window_and_fails_closed() {
        let obj = object(&[3u8; 2000]);
        let (f, counters, _) = fake_mut(std::slice::from_ref(&obj), Duration::ZERO);
        let up = upstream(f).await;
        let (cache, _role) = cache_ttl(&up.url, None, Duration::from_millis(300)).await;
        let client = RpcClient::connect(&cache.url, REPO, token()).unwrap();
        assert_eq!(fetch_all(&client, &[obj.0]).await.unwrap().len(), 1);
        let has = counters.has.load(Ordering::SeqCst);
        // Inside the window the fetch confirmed it: no upstream call at all.
        assert_eq!(fetch_all(&client, &[obj.0]).await.unwrap().len(), 1);
        assert_eq!(counters.has.load(Ordering::SeqCst), has);
        assert_eq!(counters.gets.load(Ordering::SeqCst), 1);
        // After it, one batched presence check, and no second download.
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(fetch_all(&client, &[obj.0]).await.unwrap().len(), 1);
        assert_eq!(counters.has.load(Ordering::SeqCst), has + 1);
        assert_eq!(counters.gets.load(Ordering::SeqCst), 1);
        // With the upstream gone the unconfirmed object is refused, not served.
        tokio::time::sleep(Duration::from_millis(350)).await;
        drop(up);
        assert!(fetch_all(&client, &[obj.0]).await.is_err());
        assert!(client.has_objects(&[obj.0]).await.is_err());
    }

    #[test]
    fn a_disk_directory_of_another_layout_is_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let obj = object(b"from the old layout");
        // The old directory name was the bare digest of the repository name.
        let old = hex(&Sha256::digest(REPO.as_bytes()));
        let old_file = dir.path().join(&old[..REPO_DIR_LEN]).join(obj.0.path());
        std::fs::create_dir_all(old_file.parent().unwrap()).unwrap();
        std::fs::write(&old_file, &obj.1).unwrap();
        assert_ne!(
            &old[..REPO_DIR_LEN],
            DiskCache::rel(REPO, &obj.0).split('/').next().unwrap()
        );

        let disk = DiskCache::open(dir.path(), 1 << 20).unwrap();
        assert_eq!(disk.total(), 0);
        assert!(!old_file.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(LAYOUT_FILE)).unwrap(),
            LAYOUT
        );
        // The same layout is kept across a reopen.
        let rel = DiskCache::rel(REPO, &obj.0);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(disk.put(&rel, &obj.1));
        drop(disk);
        assert_eq!(
            DiskCache::open(dir.path(), 1 << 20).unwrap().total(),
            obj.1.len() as u64
        );
    }

    #[tokio::test]
    async fn the_upstream_must_be_https_or_local() {
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        assert!(Upstream::connect("http://127.0.0.1:1").is_ok());
        assert!(Upstream::connect("https://primary.example").is_ok());
        assert!(matches!(
            Upstream::connect("http://primary.example"),
            Err(ClientError::Refused(
                asc_core::transport_policy::TransportError::PlainHttp { .. }
            ))
        ));
        assert!(Upstream::connect("ftp://x").is_err());
    }

    #[test]
    fn a_symlinked_cache_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&real, &link).is_err() {
            return;
        }
        assert!(DiskCache::open(&link, 1000).is_err());
    }

    // U78: the admin token is taken as Bearer or Basic, and a refusal is a Basic-first 401.
    #[tokio::test]
    async fn the_admin_listener_takes_bearer_or_basic_and_challenges_with_basic_first() {
        use base64::Engine as _;
        use tower::ServiceExt;
        let basic = |raw: &str| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(raw)
            )
        };
        let get = |auth: Option<String>| async move {
            let app = admin_router(Arc::new(Metrics::new()), Some(Arc::from("s3cret")));
            let mut req = axum::http::Request::get("/metrics");
            if let Some(a) = auth {
                req = req.header(axum::http::header::AUTHORIZATION, a);
            }
            app.oneshot(req.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap()
        };
        for ok in [
            "Bearer s3cret".to_owned(),
            basic("admin:s3cret"),
            basic(":s3cret"),
            basic("s3cret:"),
        ] {
            assert_eq!(get(Some(ok.clone())).await.status(), 200, "{ok}");
        }
        for bad in [
            None,
            Some("Bearer wrong".to_owned()),
            Some(basic("admin:wrong")),
            Some("Basic !!!".to_owned()),
            Some("Digest abc".to_owned()),
        ] {
            let res = get(bad.clone()).await;
            assert_eq!(res.status(), 401, "{bad:?}");
            let challenges: Vec<_> = res
                .headers()
                .get_all(axum::http::header::WWW_AUTHENTICATE)
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect();
            assert!(challenges[0].starts_with("Basic realm="), "{challenges:?}");
            assert!(challenges[1].starts_with("Bearer realm="), "{challenges:?}");
        }
    }
}
