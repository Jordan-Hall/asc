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
use std::time::{Instant, SystemTime};

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
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};
use tonic::{Code, Status};

use crate::config::{CacheConfig, ConfigError, Role, ServeConfig};
use crate::forgejo::{NEGATIVE_TTL, POSITIVE_TTL};
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

/// Plaintext is allowed only for the local machine, because it would send tokens in the clear.
fn loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// One shared, lazily connected channel to the upstream server.
pub struct Upstream {
    channel: Channel,
}

impl Upstream {
    pub fn connect(url: &str) -> Result<Self, ClientError> {
        let parsed = url::Url::parse(url).map_err(|_| ClientError::InvalidUrl(url.to_owned()))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| ClientError::InvalidUrl(url.to_owned()))?;
        let mut endpoint = Endpoint::from_shared(url.to_owned())?;
        match parsed.scheme() {
            "https" => {
                endpoint = endpoint.tls_config(ClientTlsConfig::new().with_enabled_roots())?;
            }
            "http" if loopback(host) => {}
            "http" => return Err(ClientError::Insecure(host.to_owned())),
            _ => return Err(ClientError::InvalidUrl(url.to_owned())),
        }
        let channel = endpoint
            .connect_timeout(std::time::Duration::from_secs(10))
            .tcp_nodelay(true)
            .http2_keep_alive_interval(std::time::Duration::from_secs(30))
            .keep_alive_while_idle(true)
            .connect_lazy();
        Ok(Self { channel })
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
        let dir = hex(&Sha256::digest(repo.as_bytes()));
        format!("{}/{}", &dir[..REPO_DIR_LEN], key.path())
    }

    /// Bytes in use.
    pub fn total(&self) -> u64 {
        self.lock().total
    }

    /// True when the entry is indexed (and marks it recently used). No file access.
    fn contains(&self, rel: &str) -> bool {
        self.lock().touch(rel)
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
        let mut present = vec![false; keys.len()];
        let mut unknown = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            // Objects are immutable, so a cached one is still there upstream.
            let local = inner.mem_get(&repo, *k).is_some()
                || inner
                    .disk
                    .as_ref()
                    .is_some_and(|d| d.contains(&DiskCache::rel(&repo, k)));
            if local {
                present[i] = true;
            } else {
                unknown.push(i);
            }
        }
        inner.count("presence_hit", (keys.len() - unknown.len()) as u64);
        if !unknown.is_empty() {
            let token = inner.token_of(ctx)?;
            let client = inner
                .upstream
                .client(&repo, token)
                .map_err(|_| Status::unavailable("upstream unavailable"))?;
            let asked: Vec<ObjKey> = unknown.iter().map(|&i| keys[i]).collect();
            let have = client.has_objects(&asked).await.map_err(call_status)?;
            for (i, h) in unknown.into_iter().zip(have) {
                present[i] = h;
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
        let token = self.inner.token_of(ctx)?;
        let windows: Vec<Vec<ObjKey>> = keys.chunks(WINDOW).map(<[ObjKey]>::to_vec).collect();
        let inner = self.inner.clone();
        let out = stream::iter(windows)
            .map(move |window| {
                let (inner, repo, token) = (inner.clone(), repo.clone(), token.clone());
                async move {
                    let got = inner.resolve(&repo, token, &window).await?;
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

/// The admin listener of a cache server: `/metrics`, behind the admin token when one is set.
fn admin_router(metrics: Arc<Metrics>, token: Option<Arc<str>>) -> axum::Router {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";
    axum::Router::new()
        .route(
            "/metrics",
            axum::routing::get(
                |axum::extract::State(m): axum::extract::State<Arc<Metrics>>| async move {
                    ([(header::CONTENT_TYPE, OPENMETRICS)], m.render())
                },
            ),
        )
        .with_state(metrics)
        .layer(axum::middleware::from_fn_with_state(
            token,
            |axum::extract::State(token): axum::extract::State<Option<Arc<str>>>,
             req: axum::extract::Request,
             next: axum::middleware::Next| async move {
                if let Some(expected) = token {
                    let given = req
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or("");
                    // Equal-length digests, compared without an early exit.
                    let (a, b) = (Sha256::digest(given), Sha256::digest(expected.as_bytes()));
                    let diff = a.iter().zip(b.iter()).fold(0u8, |d, (x, y)| d | (x ^ y));
                    if std::hint::black_box(diff) != 0 {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(header::WWW_AUTHENTICATE, "Bearer")],
                        )
                            .into_response();
                    }
                }
                next.run(req).await
            },
        ))
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
        objects: HashMap<ObjKey, Bytes>,
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
                if self.objects.contains_key(&k) {
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
                match self.objects.get(&key) {
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
        let role = CacheRole::new(upstream_url, &settings(dir), Arc::new(Metrics::new())).unwrap();
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

    fn fake(objects: &[(ObjKey, Bytes)], delay: Duration) -> (Fake, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        (
            Fake {
                objects: objects.iter().cloned().collect(),
                counters: counters.clone(),
                delay,
            },
            counters,
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
            } else {
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

    #[tokio::test]
    async fn the_upstream_must_be_https_or_local() {
        assert!(Upstream::connect("http://127.0.0.1:1").is_ok());
        assert!(Upstream::connect("https://primary.example").is_ok());
        assert!(matches!(
            Upstream::connect("http://primary.example"),
            Err(ClientError::Insecure(_))
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
}
