//! The Actions cache and artifact store (SPEC §12.7, PLAN U75): `/actions/v1/*`.
//!
//! A manifest (`ActionsManifest`, postcard) names the files of one cache or artifact by FileId;
//! the content itself lives in the repository namespace as ordinary objects. Manifests are stored
//! at `actions/<owner>.<repo>/<cache|artifact>/<scope>/<pct(key)>` where `pct` percent-encodes
//! every byte except `A-Z a-z 0-9 . _ -`, so a key prefix is a listing prefix.
//!
//! Scopes are the ones of the turbo cache (SPEC §12.4) and come from the same code: reads try the
//! caller's scope and then `trusted`; a write goes only to the caller's scope, so a pull request
//! cannot write `trusted`. Keys are immutable (a second write is `409`), except that an artifact
//! may be replaced on request. Retention follows the turbo settings (`pr-*` and `dev-*` 14 days,
//! `trusted` 60 days, by last access). Every unexpired manifest is a GC root for its FileIds
//! (`ActionsRoots`, SPEC §3.6).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use asc_core::content::CHUNK_THRESHOLD;
use asc_core::id::FileId;
use asc_core::object::Kind;
use asc_core::path::{RepoPath, checkout_safe};
use asc_rpc::auth::{Access, AuthError};
use asc_rpc::server::RepoName;
use asc_store::gc::{GcError, GcRootSource};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions};
use serde::{Deserialize, Serialize};

use crate::registry::{Extensions, Registry};
use crate::turbo::{Scope, Slug, TurboError, TurboState};

/// The only manifest version.
pub const MANIFEST_VERSION: u8 = 1;
/// Largest accepted or read manifest.
pub const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
/// Most entries in one manifest.
pub const MAX_ENTRIES: usize = 200_000;
/// Largest sum of entry sizes in one manifest.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_LINK_BYTES: u64 = 4096;
/// Longest percent-encoded key.
pub const MAX_KEY_PCT: usize = 200;
/// Longest prefix query.
const MAX_LIST: usize = 1000;
/// Most store entries one listing looks at.
const MAX_SCAN: usize = 100_000;
/// Keys per presence query.
const CHECK_BATCH: usize = 1000;
/// A hit older than this refreshes the access time (the object is written again).
const TOUCH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// One cache or artifact. The layout is pinned by SPEC §12.7.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsManifest {
    pub version: u8,
    pub entries: Vec<(RepoPath, Kind, FileId, u64)>,
}

impl ActionsManifest {
    /// Decodes and validates bytes from a client: version 1, bounded, strictly ascending
    /// checkout-safe paths, file, executable and symlink kinds only, bounded sizes, and no entry
    /// beneath another.
    pub fn decode(bytes: &[u8]) -> Result<Self, ActionsError> {
        let bad = |m: &str| ActionsError::BadManifest(m.to_owned());
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ActionsError::TooLarge);
        }
        let (m, rest): (Self, &[u8]) =
            postcard::take_from_bytes(bytes).map_err(|_| bad("not a manifest"))?;
        if !rest.is_empty() {
            return Err(bad("trailing bytes"));
        }
        if m.version != MANIFEST_VERSION {
            return Err(bad("unsupported version"));
        }
        if m.entries.len() > MAX_ENTRIES {
            return Err(bad("too many entries"));
        }
        let mut total = 0u64;
        let mut all: HashSet<&[u8]> = HashSet::with_capacity(m.entries.len());
        for (i, (path, kind, _, size)) in m.entries.iter().enumerate() {
            if i > 0 && m.entries[i - 1].0 >= *path {
                return Err(bad("entries are not strictly ascending"));
            }
            if !checkout_safe(path) {
                return Err(bad("unsafe path"));
            }
            match kind {
                Kind::File | Kind::Exec => {}
                Kind::Symlink if *size > 0 && *size <= MAX_LINK_BYTES => {}
                _ => return Err(bad("unsupported entry kind")),
            }
            total = total
                .checked_add(*size)
                .filter(|t| *t <= MAX_TOTAL_BYTES)
                .ok_or_else(|| bad("total size is too large"))?;
            all.insert(path.as_bytes());
        }
        for (path, ..) in &m.entries {
            let mut p = path.parent();
            while let Some(dir) = p {
                if all.contains(dir.as_bytes()) {
                    return Err(bad("an entry lies beneath another entry"));
                }
                p = dir.parent();
            }
        }
        Ok(m)
    }

    /// The object keys that hold this manifest's content: the inline file, or the chunk list
    /// (whose chunks are stored with it) of a chunked one.
    pub fn content_keys(&self) -> Vec<ObjKey> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (_, _, id, size) in &self.entries {
            let key = if *size > CHUNK_THRESHOLD {
                ObjKey::ChunkList(*id)
            } else {
                ObjKey::File(*id)
            };
            if seen.insert(key) {
                out.push(key);
            }
        }
        out
    }

    /// Every key this manifest keeps alive, as GC roots (as `asc-store`'s `file_keys` marks a
    /// tree entry).
    fn root_keys(&self) -> Vec<ObjKey> {
        let mut out = Vec::new();
        for (_, _, id, size) in &self.entries {
            out.push(ObjKey::File(*id));
            if *size > CHUNK_THRESHOLD {
                out.push(ObjKey::ChunkList(*id));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------- errors

#[derive(Debug, thiserror::Error)]
pub enum ActionsError {
    #[error("{0}")]
    BadRequest(String),
    #[error("invalid manifest: {0}")]
    BadManifest(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("not found")]
    NotFound,
    #[error("key already exists")]
    Exists,
    #[error("manifest too large")]
    TooLarge,
    #[error("{0} objects named by the manifest are not in the repository")]
    MissingObjects(usize),
    #[error("store: {0}")]
    Store(#[from] object_store::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<TurboError> for ActionsError {
    fn from(e: TurboError) -> Self {
        match e {
            TurboError::BadRequest(m) => Self::BadRequest(m.to_owned()),
            TurboError::Auth(a) => Self::Auth(a),
            TurboError::NotFound => Self::NotFound,
            TurboError::TooLarge => Self::TooLarge,
            TurboError::Store(s) => Self::Store(s),
            TurboError::Config(c) => Self::Internal(c),
        }
    }
}

impl IntoResponse for ActionsError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) | Self::BadManifest(_) => StatusCode::BAD_REQUEST,
            Self::Auth(AuthError::Unauthenticated) => StatusCode::UNAUTHORIZED,
            Self::Auth(AuthError::Malformed) => StatusCode::BAD_REQUEST,
            Self::Auth(AuthError::Forbidden) => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Exists => StatusCode::CONFLICT,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::MissingObjects(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Store(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Store errors can name buckets and paths; they go to the log only.
            tracing::warn!(event = "actions.error", error = %self);
            "internal error".to_owned()
        } else {
            self.to_string()
        };
        (
            status,
            axum::Json(serde_json::json!({ "error": { "message": message } })),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------------- names

/// `cache` or `artifact`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    Cache,
    Artifact,
}

impl StoreKind {
    fn parse(s: &str) -> Result<Self, ActionsError> {
        match s {
            "cache" => Ok(Self::Cache),
            "artifact" => Ok(Self::Artifact),
            _ => Err(ActionsError::NotFound),
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Artifact => "artifact",
        }
    }
}

/// Percent-encodes every byte except `A-Z a-z 0-9 . _ -`.
pub fn pct(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn unpct_bytes(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(hex(*b.get(i + 1)?)? << 4 | hex(*b.get(i + 2)?)?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// The key behind a stored file name: the bucket layer encodes `%` (and a lone `.`/`..`) once
/// more, then `pct` itself is undone.
fn key_of_file_name(name: &str) -> Option<String> {
    let once = String::from_utf8(unpct_bytes(name)?).ok()?;
    String::from_utf8(unpct_bytes(&once)?).ok()
}

/// A key or prefix: not empty (unless `allow_empty`), no control characters, at most
/// [`MAX_KEY_PCT`] encoded bytes.
fn check_key(key: &str, allow_empty: bool) -> Result<(), ActionsError> {
    if (key.is_empty() && !allow_empty) || key.chars().any(char::is_control) {
        return Err(ActionsError::BadRequest("invalid key".into()));
    }
    if pct(key).len() > MAX_KEY_PCT {
        return Err(ActionsError::BadRequest("key is too long".into()));
    }
    Ok(())
}

fn now_secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------------- content

/// Presence of repository objects (a manifest may name only objects the repository holds).
#[async_trait]
pub trait RepoContent: Send + Sync {
    /// Presence of each key in `repo`'s namespace; `None` when there is no such repository.
    async fn has_many(&self, repo: &str, keys: &[ObjKey]) -> Result<Option<Vec<bool>>, String>;
}

/// [`RepoContent`] over the registry.
pub struct RegistryContent(pub Arc<Registry>);

#[async_trait]
impl RepoContent for RegistryContent {
    async fn has_many(&self, repo: &str, keys: &[ObjKey]) -> Result<Option<Vec<bool>>, String> {
        let name = RepoName::parse(repo).map_err(|e| e.to_string())?;
        match self.0.open(&name).await {
            Ok(entry) => entry
                .objects
                .has_many(keys)
                .await
                .map(Some)
                .map_err(|e| e.to_string()),
            Err(crate::registry::RegistryError::NotFound(_)) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Maps a repository namespace to its `owner/repo` name (for the GC root source).
#[async_trait]
pub trait NsNames: Send + Sync {
    async fn repo_name(&self, ns: &[u8]) -> Result<Option<String>, String>;
}

/// [`NsNames`] over the `repos` table.
pub struct SqlNsNames(pub sqlx::AnyPool);

#[async_trait]
impl NsNames for SqlNsNames {
    async fn repo_name(&self, ns: &[u8]) -> Result<Option<String>, String> {
        use sqlx::Row;
        let row = sqlx::query("SELECT name FROM repos WHERE ns = $1")
            .bind(ns.to_vec())
            .fetch_optional(&self.0)
            .await
            .map_err(|e| e.to_string())?;
        row.map(|r| r.try_get::<String, _>("name").map_err(|e| e.to_string()))
            .transpose()
    }
}

// ---------------------------------------------------------------------------------- state

/// One manifest found in the store.
struct Found {
    scope: String,
    kind: StoreKind,
    key: String,
    path: StorePath,
    modified: SystemTime,
    size: u64,
}

/// Counts of a retention sweep.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub scanned: u64,
    pub deleted: u64,
}

/// Shared state of the `/actions/v1` routes.
pub struct ActionsStore {
    turbo: Arc<TurboState>,
    store: Arc<dyn ObjectStore>,
    root: StorePath,
    content: Arc<dyn RepoContent>,
    trusted: Duration,
    other: Duration,
}

impl ActionsStore {
    /// Shares the bucket, the scope rules and the retention settings of `turbo`.
    pub fn new(turbo: Arc<TurboState>, content: Arc<dyn RepoContent>) -> Self {
        let (store, root, cfg) = turbo.backing();
        let (trusted, other) = (
            DAY * cfg.trusted_retention_days,
            DAY * cfg.pr_retention_days,
        );
        Self {
            turbo,
            store,
            root,
            content,
            trusted,
            other,
        }
    }

    fn retention(&self, scope_dir: &str) -> Duration {
        if scope_dir == "trusted" {
            self.trusted
        } else {
            self.other
        }
    }

    fn expired(&self, scope_dir: &str, modified: SystemTime, now: SystemTime) -> bool {
        now.duration_since(modified).unwrap_or_default() > self.retention(scope_dir)
    }

    fn kind_dir(&self, slug_dir: &str, kind: StoreKind) -> StorePath {
        self.root
            .clone()
            .join("actions")
            .join(slug_dir)
            .join(kind.dir())
    }

    fn scope_dir(&self, slug_dir: &str, kind: StoreKind, scope_dir: &str) -> StorePath {
        self.kind_dir(slug_dir, kind).join(scope_dir)
    }

    fn key_path(&self, slug_dir: &str, kind: StoreKind, scope_dir: &str, key: &str) -> StorePath {
        self.scope_dir(slug_dir, kind, scope_dir).join(pct(key))
    }

    /// The caller's scope, then `trusted`.
    fn read_scopes(scope: &Scope) -> Vec<String> {
        let mut v = vec![scope.dir()];
        if *scope != Scope::Trusted {
            v.push(Scope::Trusted.dir());
        }
        v
    }

    /// Lists the manifests below `prefix` (`actions/…`), at most [`MAX_SCAN`]. `skip` is the
    /// number of path parts down to and including `actions`.
    async fn scan(&self, prefix: &StorePath) -> Result<Vec<(Vec<String>, Found)>, ActionsError> {
        let base = self.root.clone().join("actions");
        let skip = base.parts().count();
        let mut out = Vec::new();
        let mut list = self.store.list(Some(prefix));
        while let Some(obj) = list.try_next().await? {
            if out.len() >= MAX_SCAN {
                tracing::warn!(event = "actions.scan_truncated", scan = MAX_SCAN);
                break;
            }
            let parts: Vec<String> = obj
                .location
                .parts()
                .skip(skip)
                .map(|p| p.as_ref().to_owned())
                .collect();
            let [_, kind, scope, name] = parts.as_slice() else {
                continue;
            };
            let kind = match kind.as_str() {
                "cache" => StoreKind::Cache,
                "artifact" => StoreKind::Artifact,
                _ => continue,
            };
            let Some(key) = key_of_file_name(name) else {
                continue;
            };
            out.push((
                parts.clone(),
                Found {
                    scope: scope.clone(),
                    kind,
                    key,
                    path: obj.location.clone(),
                    modified: SystemTime::from(obj.last_modified),
                    size: obj.size,
                },
            ));
        }
        Ok(out)
    }

    async fn read_manifest_bytes(&self, path: &StorePath) -> Result<Option<Bytes>, ActionsError> {
        let got = match self.store.get(path).await {
            Ok(g) => g,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if got.meta.size > MAX_MANIFEST_BYTES as u64 {
            return Ok(None);
        }
        Ok(Some(got.bytes().await?))
    }

    /// Writes a validated manifest into the caller's scope.
    async fn put(
        &self,
        slug: &Slug,
        scope: &Scope,
        kind: StoreKind,
        key: &str,
        body: Bytes,
        overwrite: bool,
    ) -> Result<(), ActionsError> {
        let manifest = ActionsManifest::decode(&body)?;
        // The content must already be in the repository: the client uploads it first.
        let keys = manifest.content_keys();
        let repo = slug.repo_name();
        let mut missing = 0usize;
        for batch in keys.chunks(CHECK_BATCH) {
            let have = self
                .content
                .has_many(&repo, batch)
                .await
                .map_err(ActionsError::Internal)?
                .ok_or(ActionsError::NotFound)?;
            if have.len() != batch.len() {
                return Err(ActionsError::Internal("short presence answer".into()));
            }
            missing += have.iter().filter(|p| !**p).count();
        }
        if missing > 0 {
            return Err(ActionsError::MissingObjects(missing));
        }
        let scope_dir = scope.dir();
        let path = self.key_path(&slug.dir(), kind, &scope_dir, key);
        if overwrite && kind == StoreKind::Artifact {
            self.store.put(&path, body.into()).await?;
            return Ok(());
        }
        // An expired manifest is gone as far as callers can tell; the sweep has not run yet.
        match self.store.head(&path).await {
            Ok(h)
                if self.expired(
                    &scope_dir,
                    SystemTime::from(h.last_modified),
                    SystemTime::now(),
                ) =>
            {
                let _ = self.store.delete(&path).await;
            }
            Ok(_) => return Err(ActionsError::Exists),
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        let opts = PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        match self.store.put_opts(&path, body.into(), opts).await {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => Err(ActionsError::Exists),
            Err(e) => Err(e.into()),
        }
    }

    /// The manifest at `key`: the caller's scope, then `trusted`. Refreshes the access time of
    /// an old hit.
    async fn get(
        &self,
        slug: &Slug,
        scope: &Scope,
        kind: StoreKind,
        key: &str,
    ) -> Result<Option<(Bytes, String)>, ActionsError> {
        let now = SystemTime::now();
        for scope_dir in Self::read_scopes(scope) {
            let path = self.key_path(&slug.dir(), kind, &scope_dir, key);
            let head = match self.store.head(&path).await {
                Ok(h) => h,
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(e) => return Err(e.into()),
            };
            let modified = SystemTime::from(head.last_modified);
            if self.expired(&scope_dir, modified, now) {
                continue;
            }
            let Some(body) = self.read_manifest_bytes(&path).await? else {
                continue;
            };
            self.touch(&path, &body, modified, now).await;
            return Ok(Some((body, scope_dir)));
        }
        Ok(None)
    }

    async fn touch(&self, path: &StorePath, body: &Bytes, modified: SystemTime, now: SystemTime) {
        if now.duration_since(modified).unwrap_or_default() > TOUCH_AFTER
            && let Err(e) = self.store.put(path, body.clone().into()).await
        {
            tracing::debug!(event = "actions.touch_failed", error = %e);
        }
    }

    /// Unexpired manifests whose key starts with `prefix`, per scope, newest first.
    async fn matching(
        &self,
        slug: &Slug,
        scope: &str,
        kind: StoreKind,
        prefix: &str,
        now: SystemTime,
    ) -> Result<Vec<Found>, ActionsError> {
        let dir = self.scope_dir(&slug.dir(), kind, scope);
        let mut rows: Vec<Found> = self
            .scan(&dir)
            .await?
            .into_iter()
            .map(|(_, f)| f)
            .filter(|f| f.key.starts_with(prefix) && !self.expired(&f.scope, f.modified, now))
            .collect();
        rows.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.key.cmp(&a.key)));
        Ok(rows)
    }

    /// The newest manifest whose key starts with `prefix`: the caller's scope first, then
    /// `trusted` (`actions/cache` restore keys).
    async fn find_prefix(
        &self,
        slug: &Slug,
        scope: &Scope,
        kind: StoreKind,
        prefix: &str,
    ) -> Result<Option<(String, Bytes)>, ActionsError> {
        let now = SystemTime::now();
        for scope_dir in Self::read_scopes(scope) {
            for f in self.matching(slug, &scope_dir, kind, prefix, now).await? {
                if let Some(body) = self.read_manifest_bytes(&f.path).await? {
                    self.touch(&f.path, &body, f.modified, now).await;
                    return Ok(Some((f.key, body)));
                }
            }
        }
        Ok(None)
    }

    async fn list(
        &self,
        slug: &Slug,
        scope: &Scope,
        kind: StoreKind,
        prefix: &str,
    ) -> Result<Vec<serde_json::Value>, ActionsError> {
        let now = SystemTime::now();
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        for scope_dir in Self::read_scopes(scope) {
            for f in self.matching(slug, &scope_dir, kind, prefix, now).await? {
                if seen.insert(f.key.clone()) {
                    rows.push(f);
                }
            }
        }
        rows.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.key.cmp(&a.key)));
        rows.truncate(MAX_LIST);
        Ok(rows
            .into_iter()
            .map(|f| {
                serde_json::json!({
                    "key": f.key,
                    "at": now_secs(f.modified),
                    "size": f.size,
                })
            })
            .collect())
    }

    /// Deletes manifests older than their scope's retention.
    pub async fn sweep(&self, now: SystemTime) -> Result<SweepStats, ActionsError> {
        let prefix = self.root.clone().join("actions");
        let mut stats = SweepStats::default();
        for (_, f) in self.scan(&prefix).await? {
            stats.scanned += 1;
            if self.expired(&f.scope, f.modified, now) {
                match self.store.delete(&f.path).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => stats.deleted += 1,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(stats)
    }

    /// Runs [`sweep`](Self::sweep) once a day until aborted.
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(2 * 3600),
                SWEEP_EVERY,
            );
            loop {
                tick.tick().await;
                match me.sweep(SystemTime::now()).await {
                    Ok(s) => tracing::info!(
                        event = "actions.sweep",
                        scanned = s.scanned,
                        deleted = s.deleted
                    ),
                    Err(e) => tracing::warn!(event = "actions.sweep_failed", error = %e),
                }
            }
        })
    }

    /// Every FileId (as object keys) of the unexpired manifests of `slug_dir` at `now`.
    pub async fn roots_at(
        &self,
        slug_dir: &str,
        now: SystemTime,
    ) -> Result<Vec<ObjKey>, ActionsError> {
        let prefix = self.root.clone().join("actions").join(slug_dir);
        let mut seen: HashSet<ObjKey> = HashSet::new();
        for (_, f) in self.scan(&prefix).await? {
            if self.expired(&f.scope, f.modified, now) {
                continue;
            }
            let Some(body) = self.read_manifest_bytes(&f.path).await? else {
                continue;
            };
            // A damaged manifest stops the collection instead of dropping its roots.
            let manifest = ActionsManifest::decode(&body).map_err(|e| {
                ActionsError::Internal(format!("{}/{}: {e}", f.kind.dir(), f.scope))
            })?;
            seen.extend(manifest.root_keys());
        }
        Ok(seen.into_iter().collect())
    }
}

/// The GC root source: the FileIds of every unexpired Actions manifest of a repository.
pub struct ActionsRoots {
    store: Arc<ActionsStore>,
    names: Arc<dyn NsNames>,
}

impl ActionsRoots {
    pub fn new(store: Arc<ActionsStore>, names: Arc<dyn NsNames>) -> Self {
        Self { store, names }
    }
}

#[async_trait]
impl GcRootSource for ActionsRoots {
    async fn roots(&self, ns: &[u8]) -> Result<Vec<ObjKey>, GcError> {
        let fail = |e: &dyn std::fmt::Display| GcError::Other(format!("actions manifests: {e}"));
        let Some(name) = self.names.repo_name(ns).await.map_err(|e| fail(&e))? else {
            return Ok(Vec::new());
        };
        // Only names the routes can serve have manifests (an owner with a dot cannot).
        let Some((owner, repo)) = name.split_once('/') else {
            return Ok(Vec::new());
        };
        if owner.contains('.') {
            return Ok(Vec::new());
        }
        self.store
            .roots_at(&format!("{owner}.{repo}"), SystemTime::now())
            .await
            .map_err(|e| fail(&e))
    }
}

// ---------------------------------------------------------------------------------- routes

/// `owner` and `repo` of the path as the slug the authorizer is asked about. An owner with a dot
/// is refused: `a.b/c` and `a/b.c` would share the directory `a.b.c`.
fn slug_of(owner: &str, repo: &str) -> Result<String, ActionsError> {
    if owner.contains('.') {
        return Err(ActionsError::BadRequest("invalid repository name".into()));
    }
    Ok(format!("{owner}.{repo}"))
}

#[derive(Debug, Deserialize)]
struct PutQuery {
    overwrite: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FindQuery {
    prefix: Option<String>,
    list: Option<String>,
}

fn octets(body: Bytes, scope: &str, key: Option<&str>) -> Response {
    let mut res = (StatusCode::OK, Body::from(body)).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(scope) {
        h.insert("x-asc-scope", v);
    }
    if let Some(k) = key
        && let Ok(v) = HeaderValue::from_str(&pct(k))
    {
        h.insert("x-asc-key", v);
    }
    res
}

async fn put_key(
    State(s): State<Arc<ActionsStore>>,
    Path((owner, repo, kind, key)): Path<(String, String, String, String)>,
    Query(q): Query<PutQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ActionsError> {
    let kind = StoreKind::parse(&kind)?;
    check_key(&key, false)?;
    let (slug, scope) = s
        .turbo
        .gate_slug(&headers, &slug_of(&owner, &repo)?, Access::Write)
        .await?;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_MANIFEST_BYTES) {
        return Err(ActionsError::TooLarge);
    }
    let bytes = axum::body::to_bytes(body, MAX_MANIFEST_BYTES)
        .await
        .map_err(|_| ActionsError::TooLarge)?;
    let overwrite = q.overwrite.as_deref() == Some("1");
    s.put(&slug, &scope, kind, &key, bytes, overwrite).await?;
    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({ "key": key, "scope": scope.dir() })),
    )
        .into_response())
}

async fn get_key(
    State(s): State<Arc<ActionsStore>>,
    Path((owner, repo, kind, key)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ActionsError> {
    let kind = StoreKind::parse(&kind)?;
    check_key(&key, false)?;
    let (slug, scope) = s
        .turbo
        .gate_slug(&headers, &slug_of(&owner, &repo)?, Access::Read)
        .await?;
    match s.get(&slug, &scope, kind, &key).await? {
        Some((body, from)) => Ok(octets(body, &from, Some(&key))),
        None => Err(ActionsError::NotFound),
    }
}

async fn find_keys(
    State(s): State<Arc<ActionsStore>>,
    Path((owner, repo, kind)): Path<(String, String, String)>,
    Query(q): Query<FindQuery>,
    headers: HeaderMap,
) -> Result<Response, ActionsError> {
    let kind = StoreKind::parse(&kind)?;
    let prefix = q.prefix.unwrap_or_default();
    let listing = q.list.as_deref() == Some("1");
    check_key(&prefix, listing)?;
    let (slug, scope) = s
        .turbo
        .gate_slug(&headers, &slug_of(&owner, &repo)?, Access::Read)
        .await?;
    if listing {
        let rows = s.list(&slug, &scope, kind, &prefix).await?;
        return Ok(axum::Json(rows).into_response());
    }
    match s.find_prefix(&slug, &scope, kind, &prefix).await? {
        Some((key, body)) => Ok(octets(body, &scope.dir(), Some(&key))),
        None => Err(ActionsError::NotFound),
    }
}

/// The `/actions/v1` routes.
pub fn router(state: Arc<ActionsStore>) -> axum::Router {
    let routes = axum::Router::new()
        .route(
            "/actions/v1/{owner}/{repo}/{kind}/{key}",
            get(get_key).put(put_key),
        )
        .route("/actions/v1/{owner}/{repo}/{kind}", get(find_keys))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_MANIFEST_BYTES))
        .with_state(state);
    crate::authn::guard(routes, crate::authn::Surface::Api)
}

/// One registration line in `lib.rs`: mounts the routes, registers the GC root source and
/// returns the store (for its sweeper).
pub fn register(
    ext: &Extensions,
    turbo: &Arc<TurboState>,
    registry: &Arc<Registry>,
) -> Arc<ActionsStore> {
    let state = Arc::new(ActionsStore::new(
        turbo.clone(),
        Arc::new(RegistryContent(registry.clone())),
    ));
    ext.http.register(router(state.clone()));
    ext.pipeline.gc_roots().register(Arc::new(ActionsRoots::new(
        state.clone(),
        Arc::new(SqlNsNames(registry.pool().clone())),
    )));
    state
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use asc_rpc::auth::StaticTokens;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::gc::{BucketGc, GcConfig};
    use axum::http::Request;
    use object_store::memory::InMemory;
    use tower::ServiceExt;

    use super::*;
    use crate::config::TurboConfig;
    use crate::http::Metrics;
    use crate::turbo::ScopeSource;

    /// The repository's objects: a fixed set of present keys.
    #[derive(Default)]
    struct Present(Mutex<HashSet<ObjKey>>);

    #[async_trait]
    impl RepoContent for Present {
        async fn has_many(&self, repo: &str, keys: &[ObjKey]) -> Result<Option<Vec<bool>>, String> {
            if repo == "ghost/none" {
                return Ok(None);
            }
            let have = self.0.lock().unwrap();
            Ok(Some(keys.iter().map(|k| have.contains(k)).collect()))
        }
    }

    struct Fixed(Scope);

    #[async_trait]
    impl ScopeSource for Fixed {
        async fn scope(&self, _: &asc_rpc::auth::BearerToken, _: &str) -> Option<Scope> {
            Some(self.0.clone())
        }
    }

    fn tokens() -> StaticTokens {
        StaticTokens::new()
            .with_token("ci-secret", "ci", Access::Write)
            .with_token("dev-secret", "alice", Access::Write)
            .with_token("dev2-secret", "carol", Access::Write)
            .with_token("ro-secret", "bob", Access::Read)
    }

    struct Env {
        actions: Arc<ActionsStore>,
        mem: Arc<InMemory>,
        present: Arc<Present>,
    }

    fn env_with(cfg: TurboConfig) -> Env {
        let mem = Arc::new(InMemory::new());
        let turbo = Arc::new(
            TurboState::new(
                Arc::new(tokens()),
                mem.clone(),
                StorePath::from("root"),
                cfg,
                Arc::new(Metrics::new()),
            )
            .with_trusted_token("ci-secret"),
        );
        let present = Arc::new(Present::default());
        Env {
            actions: Arc::new(ActionsStore::new(turbo, present.clone())),
            mem,
            present,
        }
    }

    fn env() -> Env {
        env_with(TurboConfig::default())
    }

    fn manifest(files: &[(&str, &[u8])]) -> (Vec<u8>, Vec<ObjKey>) {
        let mut entries: Vec<_> = files
            .iter()
            .map(|(p, d)| {
                (
                    RepoPath::new(*p).unwrap(),
                    Kind::File,
                    FileId::of(d),
                    d.len() as u64,
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let keys = entries.iter().map(|e| ObjKey::File(e.2)).collect();
        let m = ActionsManifest {
            version: 1,
            entries,
        };
        (postcard::to_allocvec(&m).unwrap(), keys)
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    async fn send(e: &Env, method: &str, uri: &str, token: Option<&str>, body: Vec<u8>) -> Reply {
        let mut r = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            r = r.header("authorization", format!("Bearer {t}"));
        }
        let res = router(e.actions.clone())
            .oneshot(r.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (parts, body) = res.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: axum::body::to_bytes(body, usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        }
    }

    /// PUT of `key` holding one file `name` with `data`, whose content is in the repository.
    async fn put(e: &Env, token: &str, kind: &str, key: &str, data: &[u8]) -> StatusCode {
        let (body, keys) = manifest(&[("f", data)]);
        e.present.0.lock().unwrap().extend(keys);
        let uri = format!("/actions/v1/acme/web/{kind}/{}", pct(key));
        send(e, "PUT", &uri, Some(token), body).await.status
    }

    async fn get_key_of(e: &Env, token: Option<&str>, kind: &str, key: &str) -> Reply {
        send(
            e,
            "GET",
            &format!("/actions/v1/acme/web/{kind}/{}", pct(key)),
            token,
            Vec::new(),
        )
        .await
    }

    async fn restore(e: &Env, token: &str, prefix: &str) -> Option<String> {
        let r = send(
            e,
            "GET",
            &format!("/actions/v1/acme/web/cache?prefix={}", pct(prefix)),
            Some(token),
            Vec::new(),
        )
        .await;
        match r.status {
            StatusCode::OK => {
                let k = r.headers.get("x-asc-key").unwrap().to_str().unwrap();
                Some(String::from_utf8(unpct_bytes(k).unwrap()).unwrap())
            }
            StatusCode::NOT_FOUND => None,
            s => panic!("{s}"),
        }
    }

    async fn pause() {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    #[tokio::test]
    async fn restore_keys_prefix() {
        let e = env();
        // `actions/cache`: the exact key wins; a restore key is a literal prefix and the newest
        // match is chosen; percent signs and spaces are literal characters.
        for k in [
            "npm-a", "npm-b", "npm_c", "npm-aa", "yarn-1", "a b", "a%20b", "a/b",
        ] {
            assert_eq!(
                put(&e, "dev-secret", "cache", k, k.as_bytes()).await,
                StatusCode::CREATED
            );
            pause().await;
        }
        assert_eq!(
            restore(&e, "dev-secret", "npm-a").await.as_deref(),
            Some("npm-aa")
        );
        assert_eq!(
            restore(&e, "dev-secret", "npm-").await.as_deref(),
            Some("npm-aa")
        );
        assert_eq!(
            restore(&e, "dev-secret", "npm").await.as_deref(),
            Some("npm-aa")
        );
        assert_eq!(
            restore(&e, "dev-secret", "npm_").await.as_deref(),
            Some("npm_c")
        );
        assert_eq!(
            restore(&e, "dev-secret", "yarn").await.as_deref(),
            Some("yarn-1")
        );
        assert_eq!(restore(&e, "dev-secret", "pnpm").await, None);
        assert_eq!(
            restore(&e, "dev-secret", "NPM").await,
            None,
            "case-sensitive"
        );
        assert_eq!(
            restore(&e, "dev-secret", "a ").await.as_deref(),
            Some("a b")
        );
        assert_eq!(
            restore(&e, "dev-secret", "a%").await.as_deref(),
            Some("a%20b")
        );
        assert_eq!(
            restore(&e, "dev-secret", "a/").await.as_deref(),
            Some("a/b")
        );
        // Exact reads return the manifest.
        let r = get_key_of(&e, Some("dev-secret"), "cache", "npm-b").await;
        assert_eq!(r.status, StatusCode::OK);
        let m = ActionsManifest::decode(&r.body).unwrap();
        assert_eq!(m.entries[0].2, FileId::of(b"npm-b"));
        // Cache and artifact namespaces are separate.
        assert_eq!(
            get_key_of(&e, Some("dev-secret"), "artifact", "npm-b")
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        // Someone else's dev scope is invisible; `trusted` is shared.
        assert_eq!(restore(&e, "dev2-secret", "npm").await, None);
        assert_eq!(
            put(&e, "ci-secret", "cache", "npm-z", b"z").await,
            StatusCode::CREATED
        );
        assert_eq!(
            restore(&e, "dev2-secret", "npm").await.as_deref(),
            Some("npm-z")
        );
        // The caller's own scope is preferred over a newer `trusted` match.
        assert_eq!(
            restore(&e, "dev-secret", "npm-").await.as_deref(),
            Some("npm-aa")
        );
    }

    #[tokio::test]
    async fn pr_scopes_cannot_write_trusted() {
        let e = env();
        let turbo = e.actions.turbo.clone();
        turbo
            .set_scope_source(Arc::new(Fixed(Scope::Pr(7))))
            .ok()
            .unwrap();
        assert_eq!(
            put(&e, "dev-secret", "cache", "k", b"pr").await,
            StatusCode::CREATED
        );
        // It landed in pr-7 only.
        let slug = Slug::parse("acme.web").unwrap();
        let listed = e
            .actions
            .matching(&slug, "trusted", StoreKind::Cache, "", SystemTime::now())
            .await
            .unwrap();
        assert!(listed.is_empty());
        let listed = e
            .actions
            .matching(&slug, "pr-7", StoreKind::Cache, "", SystemTime::now())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        // Another PR does not see it; a trusted writer's key is not overwritten by a PR.
        let e2 = env();
        let t = put(&e2, "ci-secret", "cache", "k", b"trusted").await;
        assert_eq!(t, StatusCode::CREATED);
        e2.actions
            .turbo
            .set_scope_source(Arc::new(Fixed(Scope::Pr(8))))
            .ok()
            .unwrap();
        // The PR reads `trusted` but its write goes to pr-8, so the same key is free there.
        assert_eq!(
            get_key_of(&e2, Some("dev-secret"), "cache", "k")
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            put(&e2, "dev-secret", "cache", "k", b"evil").await,
            StatusCode::CREATED
        );
        let trusted = e2
            .actions
            .get(&slug, &Scope::Trusted, StoreKind::Cache, "k")
            .await
            .unwrap()
            .unwrap();
        let m = ActionsManifest::decode(&trusted.0).unwrap();
        assert_eq!(
            m.entries[0].2,
            FileId::of(b"trusted"),
            "trusted manifest is untouched"
        );
        // The PR's own scope is preferred when it reads.
        let mine = e2
            .actions
            .get(&slug, &Scope::Pr(8), StoreKind::Cache, "k")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mine.1, "pr-8");
        // Read-only and anonymous principals cannot write.
        assert_eq!(
            put(&e, "ro-secret", "cache", "x", b"x").await,
            StatusCode::FORBIDDEN
        );
        let (body, _) = manifest(&[("f", b"x")]);
        let r = send(&e, "PUT", "/actions/v1/acme/web/cache/x", None, body).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn keys_are_immutable_but_artifacts_can_be_replaced() {
        let e = env();
        assert_eq!(
            put(&e, "dev-secret", "cache", "k", b"1").await,
            StatusCode::CREATED
        );
        assert_eq!(
            put(&e, "dev-secret", "cache", "k", b"2").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            put(&e, "dev-secret", "artifact", "a", b"1").await,
            StatusCode::CREATED
        );
        assert_eq!(
            put(&e, "dev-secret", "artifact", "a", b"2").await,
            StatusCode::CONFLICT
        );
        let (body, keys) = manifest(&[("f", b"3")]);
        e.present.0.lock().unwrap().extend(keys);
        let r = send(
            &e,
            "PUT",
            "/actions/v1/acme/web/artifact/a?overwrite=1",
            Some("dev-secret"),
            body.clone(),
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED);
        let r = get_key_of(&e, Some("dev-secret"), "artifact", "a").await;
        assert_eq!(r.body, body);
        // `overwrite` does not apply to caches.
        let r = send(
            &e,
            "PUT",
            "/actions/v1/acme/web/cache/k?overwrite=1",
            Some("dev-secret"),
            body,
        )
        .await;
        assert_eq!(r.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn bad_manifests_names_and_missing_content_are_refused() {
        let e = env();
        let put_raw = |uri: &'static str, body: Vec<u8>| {
            let e = &e;
            async move { send(e, "PUT", uri, Some("dev-secret"), body).await.status }
        };
        let (good, keys) = manifest(&[("f", b"data")]);
        // The content is not in the repository.
        assert_eq!(
            put_raw("/actions/v1/acme/web/cache/k", good.clone()).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        e.present.0.lock().unwrap().extend(keys);
        assert_eq!(
            put_raw("/actions/v1/acme/web/cache/k", good.clone()).await,
            StatusCode::CREATED
        );
        // Unknown repository, kind, bad key, dotted owner.
        assert_eq!(
            put_raw("/actions/v1/ghost/none/cache/k", good.clone()).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            put_raw("/actions/v1/acme/web/other/k", good.clone()).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            put_raw("/actions/v1/a.b/web/cache/k", good.clone()).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            put_raw("/actions/v1/acme/web/cache/k%0Ax", good.clone()).await,
            StatusCode::BAD_REQUEST
        );
        // Garbage and structurally invalid manifests.
        assert_eq!(
            put_raw("/actions/v1/acme/web/cache/g1", vec![0xff; 40]).await,
            StatusCode::BAD_REQUEST
        );
        let mut m = ActionsManifest::decode(&good).unwrap();
        m.version = 2;
        assert_eq!(
            put_raw(
                "/actions/v1/acme/web/cache/g2",
                postcard::to_allocvec(&m).unwrap()
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        let mut m = ActionsManifest::decode(&good).unwrap();
        m.entries.push(m.entries[0].clone());
        assert_eq!(
            put_raw(
                "/actions/v1/acme/web/cache/g3",
                postcard::to_allocvec(&m).unwrap()
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        let mut m = ActionsManifest::decode(&good).unwrap();
        m.entries[0].1 = Kind::Dir;
        assert_eq!(
            put_raw(
                "/actions/v1/acme/web/cache/g4",
                postcard::to_allocvec(&m).unwrap()
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        let mut m = ActionsManifest::decode(&good).unwrap();
        m.entries.push((
            RepoPath::new("f/g").unwrap(),
            Kind::File,
            FileId::of(b"data"),
            4,
        ));
        assert_eq!(
            put_raw(
                "/actions/v1/acme/web/cache/g5",
                postcard::to_allocvec(&m).unwrap()
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        let mut big = good.clone();
        big.resize(MAX_MANIFEST_BYTES + 1, 0);
        assert_eq!(
            put_raw("/actions/v1/acme/web/cache/g6", big).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let long = "k".repeat(201);
        let uri = format!("/actions/v1/acme/web/cache/{long}");
        assert_eq!(
            send(&e, "PUT", &uri, Some("dev-secret"), good).await.status,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn listing_returns_newest_first_across_scopes() {
        let e = env();
        for k in ["run-1", "run-2", "other"] {
            assert_eq!(
                put(&e, "dev-secret", "artifact", k, k.as_bytes()).await,
                StatusCode::CREATED
            );
            pause().await;
        }
        assert_eq!(
            put(&e, "ci-secret", "artifact", "run-3", b"3").await,
            StatusCode::CREATED
        );
        let r = send(
            &e,
            "GET",
            "/actions/v1/acme/web/artifact?list=1&prefix=run-",
            Some("dev-secret"),
            Vec::new(),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&r.body).unwrap();
        let keys: Vec<&str> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["run-3", "run-2", "run-1"]);
        let r = send(
            &e,
            "GET",
            "/actions/v1/acme/web/artifact?list=1",
            Some("dev-secret"),
            Vec::new(),
        )
        .await;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(rows.len(), 4);
        // A prefix lookup needs a prefix.
        let r = send(
            &e,
            "GET",
            "/actions/v1/acme/web/cache",
            Some("dev-secret"),
            Vec::new(),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn retention_hides_and_sweeps_expired_manifests() {
        let e = env();
        assert_eq!(
            put(&e, "dev-secret", "cache", "old", b"o").await,
            StatusCode::CREATED
        );
        assert_eq!(
            put(&e, "ci-secret", "cache", "oldtrusted", b"t").await,
            StatusCode::CREATED
        );
        let slug = Slug::parse("acme.web").unwrap();
        let t = |days: u64| SystemTime::now() + DAY * (days as u32);
        // Expired for a dev scope after 14 days, for `trusted` after 60.
        let rows = e
            .actions
            .matching(&slug, "dev-alice", StoreKind::Cache, "", t(13))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let rows = e
            .actions
            .matching(&slug, "dev-alice", StoreKind::Cache, "", t(15))
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert_eq!(
            e.actions.sweep(t(15)).await.unwrap(),
            SweepStats {
                scanned: 2,
                deleted: 1
            }
        );
        assert_eq!(
            e.actions.sweep(t(61)).await.unwrap(),
            SweepStats {
                scanned: 1,
                deleted: 1
            }
        );
        assert_eq!(e.actions.sweep(t(61)).await.unwrap().scanned, 0);
        // An expired key can be written again before the sweep gets to it.
        assert_eq!(
            put(&e, "dev-secret", "cache", "k", b"1").await,
            StatusCode::CREATED
        );
        let path = e
            .actions
            .key_path("acme.web", StoreKind::Cache, "dev-alice", "k");
        let body = e.mem.get(&path).await.unwrap().bytes().await.unwrap();
        assert!(!body.is_empty());
    }

    #[test]
    fn stored_names_round_trip_to_keys() {
        for k in [
            "plain-1.0_x",
            "a b",
            "a%20b",
            "é/ü",
            ".",
            "..",
            "%",
            "a/b?c#d",
        ] {
            let mem_path = StorePath::from("r").join(pct(k));
            let name = mem_path.filename().unwrap();
            assert_eq!(key_of_file_name(name).as_deref(), Some(k), "{k}");
        }
    }

    #[tokio::test]
    async fn gc_root_keeps_the_files_of_an_unexpired_manifest() {
        // Two manifests: one in `trusted` (unexpired) and one the sweep removed. Collecting the
        // repository with this root source keeps the first manifest's files and sweeps others.
        let e = env();
        let store: Arc<dyn ObjectStore> = e.mem.clone();
        let repo_root = StorePath::from("ns/x");
        let objects = BucketObjects::new(store.clone(), repo_root.clone());
        let put_file = |data: &'static [u8]| {
            let objects = &objects;
            async move {
                let id = FileId::of(data);
                objects
                    .put(&ObjKey::File(id), encode_stored(data))
                    .await
                    .unwrap();
                id
            }
        };
        let kept = put_file(b"kept by a cache").await;
        let lost = put_file(b"referenced by nothing").await;
        let (body, keys) = manifest(&[("f", b"kept by a cache")]);
        e.present.0.lock().unwrap().extend(keys);
        assert_eq!(
            send(
                &e,
                "PUT",
                "/actions/v1/acme/web/cache/k",
                Some("ci-secret"),
                body
            )
            .await
            .status,
            StatusCode::CREATED
        );

        struct Names;
        #[async_trait]
        impl NsNames for Names {
            async fn repo_name(&self, ns: &[u8]) -> Result<Option<String>, String> {
                assert_eq!(ns, b"");
                Ok(Some("acme/web".to_owned()))
            }
        }
        let roots = ActionsRoots::new(e.actions.clone(), Arc::new(Names));
        let listed = roots.roots(b"").await.unwrap();
        assert!(listed.contains(&ObjKey::File(kept)));
        assert!(!listed.contains(&ObjKey::File(lost)));

        let gc = BucketGc::connect(store.clone(), repo_root.clone())
            .await
            .unwrap();
        let cfg = GcConfig {
            freeze_wait: Duration::ZERO,
            ..GcConfig::default()
        };
        let extra: Vec<Arc<dyn GcRootSource>> = vec![Arc::new(roots)];
        let t1 = SystemTime::now() + Duration::from_secs(15 * 24 * 3600);
        gc.run(t1, false, &cfg, &extra).await.unwrap();
        let second = gc
            .run(t1 + Duration::from_secs(25 * 3600), false, &cfg, &extra)
            .await
            .unwrap();
        assert!(!second.sweep.waiting_for_second_mark);
        let have = objects
            .has_many(&[ObjKey::File(kept), ObjKey::File(lost)])
            .await
            .unwrap();
        assert_eq!(
            have,
            [true, false],
            "the manifest's file survives, the orphan is swept"
        );
    }

    #[tokio::test]
    async fn expired_manifests_are_not_gc_roots_and_damaged_ones_stop_collection() {
        let e = env();
        let (body, keys) = manifest(&[("f", b"x")]);
        e.present.0.lock().unwrap().extend(keys.clone());
        assert_eq!(
            send(
                &e,
                "PUT",
                "/actions/v1/acme/web/cache/k",
                Some("dev-secret"),
                body
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let now = SystemTime::now();
        assert_eq!(e.actions.roots_at("acme.web", now).await.unwrap(), keys);
        assert!(
            e.actions
                .roots_at("acme.web", now + DAY * 15)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            e.actions
                .roots_at("other.repo", now)
                .await
                .unwrap()
                .is_empty()
        );
        let path = e
            .actions
            .key_path("acme.web", StoreKind::Cache, "dev-alice", "k");
        e.mem
            .put(&path, Bytes::from_static(b"junk").into())
            .await
            .unwrap();
        assert!(e.actions.roots_at("acme.web", now).await.is_err());
    }
}
