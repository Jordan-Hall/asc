//! The Turborepo remote cache on the same bucket (SPEC §12.4, PLAN U49): `/v8/artifacts/*`.
//!
//! Artefacts live at `turbo/<owner>.<repo>/<scope>/<hash>` with a `<hash>.meta` JSON sibling
//! (written last, so an upload without it is invisible). The `x-artifact-tag` signature is stored
//! and returned opaquely: turbo verifies it, the server never does. A scope is `trusted`,
//! `pr-<n>` or `dev-<user>`; reads try the caller's scope and then `trusted`, writes go only to
//! the caller's scope. Every request is authorised against the repository named by `slug`.

use std::collections::HashSet;
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path as StorePath;
use object_store::signer::Signer;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use serde::{Deserialize, Serialize};

use crate::config::{ServeConfig, TurboConfig};
use crate::http::Metrics;
use crate::registry::Extensions;

/// Largest accepted artefact.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Artefacts at least this large are offered presigned URLs on S3 (`OPTIONS`).
pub const PRESIGN_MIN_BYTES: u64 = 8 * 1024 * 1024;
/// Lifetime of a presigned URL.
const PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);
/// Largest JSON request body (batch query and events).
const MAX_JSON_BYTES: usize = 1024 * 1024;
/// Most hashes in one batch query, and most events in one post.
const MAX_ITEMS: usize = 10_000;
/// Concurrent store lookups of one batch query.
const BATCH_CONCURRENCY: usize = 16;
/// Largest `.meta` file read.
const MAX_META_BYTES: usize = 8 * 1024;
/// Multipart chunk size of an upload.
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;
/// A hit older than this refreshes the access time recorded in `.meta`.
const TOUCH_AFTER_SECS: i64 = 24 * 60 * 60;
/// Longest accepted opaque header value (tag, sha).
const MAX_HEADER_VALUE: usize = 1024;
const MAX_TRUSTED_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TRUSTED_TOKENS: usize = 1000;
const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const DAY_SECS: u64 = 24 * 60 * 60;

/// Why a turbo request or setup failed.
#[derive(Debug, thiserror::Error)]
pub enum TurboError {
    #[error("{0}")]
    BadRequest(&'static str),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("artifact not found")]
    NotFound,
    #[error("artifact too large")]
    TooLarge,
    #[error("store: {0}")]
    Store(#[from] object_store::Error),
    #[error("config: {0}")]
    Config(String),
}

impl IntoResponse for TurboError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Auth(AuthError::Unauthenticated) => StatusCode::UNAUTHORIZED,
            Self::Auth(AuthError::Malformed) => StatusCode::BAD_REQUEST,
            Self::Auth(AuthError::Forbidden) => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Store(_) | Self::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Store errors can name buckets and paths; they go to the log only.
            tracing::warn!(event = "turbo.error", error = %self);
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

/// One path segment: 1..=`max` characters of `[A-Za-z0-9_.-]`, not `.` or `..`.
fn segment_ok(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A cache namespace: `slug = <owner>.<repo>`, split at the first dot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slug {
    owner: String,
    repo: String,
}

impl Slug {
    pub fn parse(s: &str) -> Result<Self, TurboError> {
        let bad = TurboError::BadRequest("slug must be <owner>.<repo>");
        let (owner, repo) = s.split_once('.').ok_or(bad)?;
        if !segment_ok(owner, 100) || !segment_ok(repo, 100) || owner.contains('.') {
            return Err(TurboError::BadRequest("slug must be <owner>.<repo>"));
        }
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }

    /// The repository name the authorizer sees (`owner/repo`).
    pub fn repo_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn dir(&self) -> String {
        format!("{}.{}", self.owner, self.repo)
    }
}

/// A task hash. Turbo sends hex; any `[A-Za-z0-9_-]` string up to 128 bytes is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactHash(String);

impl ArtifactHash {
    pub fn parse(s: &str) -> Result<Self, TurboError> {
        let ok = !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(TurboError::BadRequest("invalid artifact hash"))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where an artefact is stored and who may overwrite it (SPEC §12.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Trusted,
    Pr(u64),
    Dev(String),
}

impl Scope {
    fn dev(principal: &str) -> Self {
        let clean: String = principal
            .chars()
            .take(64)
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self::Dev(if clean.is_empty() { "_".into() } else { clean })
    }

    fn dir(&self) -> String {
        match self {
            Self::Trusted => "trusted".into(),
            Self::Pr(n) => format!("pr-{n}"),
            Self::Dev(u) => format!("dev-{u}"),
        }
    }

    fn kind(&self) -> usize {
        match self {
            Self::Trusted => 0,
            Self::Pr(_) => 1,
            Self::Dev(_) => 2,
        }
    }
}

const KIND_NAMES: [&str; 3] = ["trusted", "pr", "dev"];

/// What a Stage 2 authz response says about a token (SPEC §12.4).
#[async_trait]
pub trait ScopeSource: Send + Sync {
    /// `Trusted` for an Actions task on a push to a protected branch, `Pr(n)` for PR `n`, and
    /// `None` for a developer PAT (which then writes to `dev-<user>`).
    async fn scope(&self, token: &BearerToken, repo: &str) -> Option<Scope>;
}

// ---------------------------------------------------------------------------------- meta

/// The `.meta` sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Meta {
    /// Task duration in milliseconds (`x-artifact-duration`).
    pub duration: u64,
    /// The opaque HMAC signature (`x-artifact-tag`).
    pub tag: Option<String>,
    pub sha: Option<String>,
    pub dirty_hash: Option<String>,
    pub size: u64,
    /// Last access, unix seconds.
    pub at: i64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn opaque_header(h: &HeaderMap, name: &str) -> Result<Option<String>, TurboError> {
    let Some(v) = h.get(name) else {
        return Ok(None);
    };
    let s = v
        .to_str()
        .map_err(|_| TurboError::BadRequest("invalid header value"))?;
    if s.len() > MAX_HEADER_VALUE || !s.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
        return Err(TurboError::BadRequest("invalid header value"));
    }
    Ok(Some(s.to_owned()))
}

// ---------------------------------------------------------------------------------- state

/// Counts of retention sweeps, for logs and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub scanned: u64,
    pub deleted: u64,
}

/// Shared state of the `/v8` routes.
pub struct TurboState {
    authorizer: Arc<dyn Authorizer>,
    store: Arc<dyn ObjectStore>,
    root: StorePath,
    signer: Option<Arc<dyn Signer>>,
    trusted: HashSet<[u8; 32]>,
    cfg: TurboConfig,
    metrics: Arc<Metrics>,
    scope_source: OnceLock<Arc<dyn ScopeSource>>,
    /// Hits and misses reported by turbo, by scope kind.
    events: [[AtomicU64; 2]; 3],
}

impl TurboState {
    pub fn new(
        authorizer: Arc<dyn Authorizer>,
        store: Arc<dyn ObjectStore>,
        root: StorePath,
        cfg: TurboConfig,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            authorizer,
            store,
            root,
            signer: None,
            trusted: HashSet::new(),
            cfg,
            metrics,
            scope_source: OnceLock::new(),
            events: Default::default(),
        }
    }

    /// Opens the state for a server config: the bucket from `store`, the trusted tokens from
    /// `turbo.trusted_tokens_file` and a presigner when the store is S3.
    pub fn open(
        cfg: &ServeConfig,
        authorizer: Arc<dyn Authorizer>,
        metrics: Arc<Metrics>,
    ) -> Result<Self, TurboError> {
        let url = cfg
            .store
            .as_deref()
            .ok_or_else(|| TurboError::Config("`store` is required".into()))?;
        let url = url::Url::parse(url).map_err(|e| TurboError::Config(e.to_string()))?;
        let (store, root, signer) = if url.scheme() == "s3" {
            let (store, root) = asc_store::s3::open_with(&url, None)
                .map_err(|e| TurboError::Config(e.to_string()))?;
            (store, root, Some(s3_signer(&url)?))
        } else {
            let (store, root) =
                object_store::parse_url(&url).map_err(|e| TurboError::Config(e.to_string()))?;
            (Arc::from(store), root, None)
        };
        let mut state = Self::new(authorizer, store, root, cfg.turbo.clone(), metrics);
        state.signer = signer;
        if let Some(file) = &cfg.turbo.trusted_tokens_file {
            state.trusted = load_trusted_tokens(file)?;
        }
        Ok(state)
    }

    /// Replaces the presigner (S3 only).
    #[must_use]
    pub fn with_signer(mut self, signer: Arc<dyn Signer>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Adds a Stage 1 trusted token.
    #[must_use]
    pub fn with_trusted_token(mut self, token: &str) -> Self {
        self.trusted
            .insert(*blake3::hash(token.as_bytes()).as_bytes());
        self
    }

    /// Installs the Stage 2 scope source, once (U44/U63).
    pub fn set_scope_source(&self, s: Arc<dyn ScopeSource>) -> Result<(), Arc<dyn ScopeSource>> {
        self.scope_source.set(s)
    }

    fn path(&self, slug: &Slug, scope: &Scope, file: &str) -> StorePath {
        self.root
            .clone()
            .join("turbo")
            .join(slug.dir())
            .join(scope.dir())
            .join(file)
    }

    fn data_path(&self, slug: &Slug, scope: &Scope, hash: &ArtifactHash) -> StorePath {
        self.path(slug, scope, hash.as_str())
    }

    fn meta_path(&self, slug: &Slug, scope: &Scope, hash: &ArtifactHash) -> StorePath {
        self.path(slug, scope, &format!("{}.meta", hash.as_str()))
    }

    /// Authorises the call and works out its scope.
    async fn gate(
        &self,
        headers: &HeaderMap,
        q: &RepoQuery,
        access: Access,
    ) -> Result<Caller, TurboError> {
        let slug = Slug::parse(
            q.slug
                .as_deref()
                .ok_or(TurboError::BadRequest("slug is required"))?,
        )?;
        let token = bearer(headers)?;
        let repo = slug.repo_name();
        let principal = self
            .authorizer
            .authorize(token.as_ref(), &repo, access)
            .await?;
        let scope = self
            .caller_scope(token.as_ref(), &principal, &repo, headers)
            .await;
        Ok(Caller { slug, scope })
    }

    async fn caller_scope(
        &self,
        token: Option<&BearerToken>,
        principal: &Principal,
        repo: &str,
        headers: &HeaderMap,
    ) -> Scope {
        if let Some(src) = self.scope_source.get() {
            return match token {
                Some(t) => src
                    .scope(t, repo)
                    .await
                    .unwrap_or_else(|| Scope::dev(&principal.id)),
                None => Scope::Trusted,
            };
        }
        if token.is_some_and(|t| {
            self.trusted
                .contains(blake3::hash(t.expose().as_bytes()).as_bytes())
        }) {
            return Scope::Trusted;
        }
        // The header is untrusted and only keeps PRs from reading each other's entries.
        let n = headers
            .get("x-asc-pr")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Scope::Pr(n)
    }

    async fn read_meta(&self, p: &StorePath) -> Result<Option<Meta>, TurboError> {
        let got = match self.store.get(p).await {
            Ok(g) => g,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if got.meta.size > MAX_META_BYTES as u64 {
            return Ok(None);
        }
        let bytes = got.bytes().await?;
        Ok(serde_json::from_slice(&bytes).ok())
    }

    async fn write_meta(&self, p: &StorePath, meta: &Meta) -> Result<(), TurboError> {
        let body = serde_json::to_vec(meta).map_err(|e| TurboError::Config(e.to_string()))?;
        self.store.put(p, body.into()).await?;
        Ok(())
    }

    /// The first scope (caller's, then `trusted`) holding a complete artefact.
    async fn find(&self, c: &Caller, hash: &ArtifactHash) -> Result<Option<Found>, TurboError> {
        let mut scopes = vec![c.scope.clone()];
        if c.scope != Scope::Trusted {
            scopes.push(Scope::Trusted);
        }
        for scope in scopes {
            let meta_path = self.meta_path(&c.slug, &scope, hash);
            let Some(mut meta) = self.read_meta(&meta_path).await? else {
                continue;
            };
            let data = self.data_path(&c.slug, &scope, hash);
            match self.store.head(&data).await {
                Ok(h) if h.size == meta.size => {}
                Ok(_) | Err(object_store::Error::NotFound { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
            let now = now_secs();
            if now - meta.at > TOUCH_AFTER_SECS {
                meta.at = now;
                if let Err(e) = self.write_meta(&meta_path, &meta).await {
                    tracing::debug!(event = "turbo.touch_failed", error = %e);
                }
            }
            return Ok(Some(Found { meta, data }));
        }
        Ok(None)
    }

    async fn presign_url(
        &self,
        method: Method,
        path: &StorePath,
    ) -> Result<Option<String>, TurboError> {
        let Some(signer) = &self.signer else {
            return Ok(None);
        };
        let url = signer.signed_url(method, path, PRESIGN_TTL).await?;
        Ok(Some(url.to_string()))
    }

    /// Deletes artefacts not accessed within their scope's retention (a daily sweep).
    pub async fn sweep(&self, now: SystemTime) -> Result<SweepStats, TurboError> {
        let prefix = self.root.clone().join("turbo");
        let skip = prefix.parts().count();
        let mut stats = SweepStats::default();
        let mut list = self.store.list(Some(&prefix));
        while let Some(obj) = list.try_next().await? {
            let parts: Vec<String> = obj
                .location
                .parts()
                .skip(skip)
                .map(|p| p.as_ref().to_owned())
                .collect();
            let [_, scope, file] = parts.as_slice() else {
                continue;
            };
            let Some(hash) = file.strip_suffix(".meta") else {
                continue;
            };
            stats.scanned += 1;
            let days = if scope == "trusted" {
                self.cfg.trusted_retention_days
            } else {
                self.cfg.pr_retention_days
            };
            let limit = Duration::from_secs(u64::from(days) * DAY_SECS);
            let age = now
                .duration_since(SystemTime::from(obj.last_modified))
                .unwrap_or_default();
            if age <= limit {
                continue;
            }
            let dir: StorePath = obj.location.parts().take(skip + 2).collect();
            let data_path = dir.join(hash);
            for p in [&data_path, &obj.location] {
                match self.store.delete(p).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            stats.deleted += 1;
        }
        Ok(stats)
    }

    /// Runs [`sweep`](Self::sweep) once a day until aborted.
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(3600),
                SWEEP_EVERY,
            );
            loop {
                tick.tick().await;
                match me.sweep(SystemTime::now()).await {
                    Ok(s) => tracing::info!(
                        event = "turbo.sweep",
                        scanned = s.scanned,
                        deleted = s.deleted
                    ),
                    Err(e) => tracing::warn!(event = "turbo.sweep_failed", error = %e),
                }
            }
        })
    }

    fn count_event(&self, scope: &Scope, source: &str, hit: bool) {
        let source = match source {
            "LOCAL" => "local",
            "REMOTE" => "remote",
            _ => "other",
        };
        let result = if hit { "hit" } else { "miss" };
        self.metrics
            .cache_requests
            .get_or_create(&vec![
                ("tier".to_owned(), "turbo".to_owned()),
                ("source".to_owned(), source.to_owned()),
                ("result".to_owned(), result.to_owned()),
            ])
            .inc();
        let k = scope.kind();
        self.events[k][usize::from(!hit)].fetch_add(1, Ordering::Relaxed);
        let hits = self.events[k][0].load(Ordering::Relaxed);
        let misses = self.events[k][1].load(Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)]
        let ratio = hits as f64 / (hits + misses) as f64;
        self.metrics
            .turbo_hit_ratio
            .get_or_create(&vec![("scope".to_owned(), KIND_NAMES[k].to_owned())])
            .set(ratio);
    }
}

fn s3_signer(url: &url::Url) -> Result<Arc<dyn Signer>, TurboError> {
    let bad = |m: String| TurboError::Config(m);
    let bucket = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| bad("s3 url has no bucket".into()))?;
    let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
    let mut region = None;
    for (k, v) in url.query_pairs() {
        match &*k {
            "endpoint" if !v.is_empty() => {
                b = b
                    .with_allow_http(v.starts_with("http://"))
                    .with_endpoint(v.to_string());
            }
            "region" if !v.is_empty() => region = Some(v.to_string()),
            "path_style" if !v.is_empty() => {
                b = b.with_virtual_hosted_style_request(!matches!(&*v, "true" | "1"));
            }
            _ => {}
        }
    }
    if let Some(r) = region {
        b = b.with_region(r);
    } else if std::env::var_os("AWS_REGION").is_none()
        && std::env::var_os("AWS_DEFAULT_REGION").is_none()
    {
        b = b.with_region("us-east-1");
    }
    Ok(Arc::new(b.build().map_err(|e| bad(e.to_string()))?))
}

/// Reads one token per line (blank lines and `#` comments skipped) as BLAKE3 digests.
pub fn load_trusted_tokens(path: &FsPath) -> Result<HashSet<[u8; 32]>, TurboError> {
    let bad = |m: String| TurboError::Config(format!("{}: {m}", path.display()));
    let file = std::fs::File::open(path).map_err(|e| bad(e.to_string()))?;
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(file, MAX_TRUSTED_FILE_BYTES + 1),
        &mut text,
    )
    .map_err(|e| bad(e.to_string()))?;
    if text.len() as u64 > MAX_TRUSTED_FILE_BYTES {
        return Err(bad("file is too large".into()));
    }
    let mut out = HashSet::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        BearerToken::new(line).map_err(|_| bad("invalid token".into()))?;
        out.insert(*blake3::hash(line.as_bytes()).as_bytes());
        if out.len() > MAX_TRUSTED_TOKENS {
            return Err(bad("too many tokens".into()));
        }
    }
    Ok(out)
}

fn bearer(headers: &HeaderMap) -> Result<Option<BearerToken>, TurboError> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AuthError::Malformed.into());
    }
    let text = first.to_str().map_err(|_| AuthError::Malformed)?;
    let (scheme, rest) = text.split_once(' ').ok_or(AuthError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::Malformed.into());
    }
    Ok(Some(BearerToken::new(rest)?))
}

struct Caller {
    slug: Slug,
    scope: Scope,
}

struct Found {
    meta: Meta,
    data: StorePath,
}

/// `?slug=<owner>.<repo>` (turbo also sends `teamId`, which is ignored).
#[derive(Debug, Deserialize)]
struct RepoQuery {
    slug: Option<String>,
    size: Option<u64>,
    method: Option<String>,
}

// ---------------------------------------------------------------------------------- handlers

async fn status() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "status": "enabled" }))
}

fn artifact_headers(meta: &Meta) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.size));
    h.insert("x-artifact-duration", HeaderValue::from(meta.duration));
    if let Some(tag) = meta
        .tag
        .as_deref()
        .and_then(|t| HeaderValue::from_str(t).ok())
    {
        h.insert("x-artifact-tag", tag);
    }
    h
}

async fn get_artifact(
    State(s): State<Arc<TurboState>>,
    method: Method,
    Path(hash): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
) -> Result<Response, TurboError> {
    let hash = ArtifactHash::parse(&hash)?;
    let caller = s.gate(&headers, &q, Access::Read).await?;
    let found = s.find(&caller, &hash).await?.ok_or(TurboError::NotFound)?;
    let out = artifact_headers(&found.meta);
    if method == Method::HEAD {
        return Ok((StatusCode::OK, out, Body::empty()).into_response());
    }
    let stream = s
        .store
        .get(&found.data)
        .await
        .map_err(|e| match e {
            object_store::Error::NotFound { .. } => TurboError::NotFound,
            e => e.into(),
        })?
        .into_stream();
    Ok((StatusCode::OK, out, Body::from_stream(stream)).into_response())
}

async fn put_artifact(
    State(s): State<Arc<TurboState>>,
    Path(hash): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, TurboError> {
    let hash = ArtifactHash::parse(&hash)?;
    let caller = s.gate(&headers, &q, Access::Write).await?;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_ARTIFACT_BYTES) {
        return Err(TurboError::TooLarge);
    }
    let data = s.data_path(&caller.slug, &caller.scope, &hash);
    let duration = opaque_header(&headers, "x-artifact-duration")?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let tag = opaque_header(&headers, "x-artifact-tag")?;
    let sha = opaque_header(&headers, "x-asc-sha")?;
    let dirty_hash = opaque_header(&headers, "x-asc-dirty-hash")?;

    let size = if headers.contains_key("x-asc-presigned") {
        // The bytes went straight to S3 through the URL from `OPTIONS`; commit what is there.
        let head = s.store.head(&data).await.map_err(|e| match e {
            object_store::Error::NotFound { .. } => TurboError::BadRequest("nothing uploaded"),
            e => e.into(),
        })?;
        if head.size > MAX_ARTIFACT_BYTES {
            let _ = s.store.delete(&data).await;
            return Err(TurboError::TooLarge);
        }
        head.size
    } else {
        upload(&s, &data, body).await?
    };
    let meta = Meta {
        duration,
        tag,
        sha,
        dirty_hash,
        size,
        at: now_secs(),
    };
    s.write_meta(&s.meta_path(&caller.slug, &caller.scope, &hash), &meta)
        .await?;
    let url = format!("{}/{}", caller.slug.dir(), hash.as_str());
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({ "urls": [url] })),
    )
        .into_response())
}

/// Streams `body` into `path` in bounded chunks; returns the stored size.
async fn upload(s: &TurboState, path: &StorePath, body: Body) -> Result<u64, TurboError> {
    let upload = s.store.put_multipart(path).await?;
    let mut w = WriteMultipart::new_with_chunk_size(upload, UPLOAD_CHUNK);
    let mut total = 0u64;
    let mut stream = body.into_data_stream();
    let result: Result<(), TurboError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| TurboError::BadRequest("body read failed"))?;
            total += chunk.len() as u64;
            if total > MAX_ARTIFACT_BYTES {
                return Err(TurboError::TooLarge);
            }
            w.wait_for_capacity(2).await?;
            w.write(&chunk);
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            w.finish().await?;
            Ok(total)
        }
        Err(e) => {
            let _ = w.abort().await;
            Err(e)
        }
    }
}

async fn options_artifact(
    State(s): State<Arc<TurboState>>,
    Path(hash): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
) -> Result<Response, TurboError> {
    let hash = ArtifactHash::parse(&hash)?;
    let mut out = serde_json::Map::new();
    out.insert("min_bytes".into(), PRESIGN_MIN_BYTES.into());
    if q.method.as_deref() == Some("put") {
        let caller = s.gate(&headers, &q, Access::Write).await?;
        let size = q.size.ok_or(TurboError::BadRequest("size is required"))?;
        if size > MAX_ARTIFACT_BYTES {
            return Err(TurboError::TooLarge);
        }
        if size >= PRESIGN_MIN_BYTES && s.signer.is_some() {
            let p = s.data_path(&caller.slug, &caller.scope, &hash);
            out.insert("put".into(), s.presign_url(Method::PUT, &p).await?.into());
        }
    } else {
        let caller = s.gate(&headers, &q, Access::Read).await?;
        if s.signer.is_some()
            && let Some(f) = s.find(&caller, &hash).await?
            && f.meta.size >= PRESIGN_MIN_BYTES
        {
            out.insert(
                "get".into(),
                s.presign_url(Method::GET, &f.data).await?.into(),
            );
        }
    }
    Ok((
        StatusCode::OK,
        [(header::ALLOW, "GET, HEAD, PUT, OPTIONS")],
        axum::Json(out),
    )
        .into_response())
}

async fn read_json<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, TurboError> {
    let bytes = axum::body::to_bytes(body, MAX_JSON_BYTES)
        .await
        .map_err(|_| TurboError::BadRequest("body too large or unreadable"))?;
    serde_json::from_slice(&bytes).map_err(|_| TurboError::BadRequest("invalid JSON body"))
}

#[derive(Deserialize)]
struct BatchRequest {
    hashes: Vec<String>,
}

async fn query_artifacts(
    State(s): State<Arc<TurboState>>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, TurboError> {
    let caller = s.gate(&headers, &q, Access::Read).await?;
    let req: BatchRequest = read_json(body).await?;
    if req.hashes.len() > MAX_ITEMS {
        return Err(TurboError::BadRequest("too many hashes"));
    }
    let hashes = req
        .hashes
        .iter()
        .map(|h| ArtifactHash::parse(h))
        .collect::<Result<Vec<_>, _>>()?;
    let rows: Vec<_> = futures::stream::iter(hashes)
        .map(|h| {
            let (s, caller) = (&s, &caller);
            async move {
                let v = match s.find(caller, &h).await {
                    Ok(Some(f)) => serde_json::json!({
                        "size": f.meta.size,
                        "taskDurationMs": f.meta.duration,
                        "tag": f.meta.tag,
                    }),
                    Ok(None) => serde_json::json!({ "error": { "message": "Artifact not found" } }),
                    Err(e) => {
                        tracing::warn!(event = "turbo.query_failed", error = %e);
                        serde_json::json!({ "error": { "message": "lookup failed" } })
                    }
                };
                (h.as_str().to_owned(), v)
            }
        })
        .buffered(BATCH_CONCURRENCY)
        .collect()
        .await;
    Ok(axum::Json(serde_json::Value::Object(rows.into_iter().collect())).into_response())
}

#[derive(Deserialize)]
struct CacheEvent {
    #[serde(default)]
    source: String,
    #[serde(default)]
    event: String,
}

async fn post_events(
    State(s): State<Arc<TurboState>>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, TurboError> {
    let caller = s.gate(&headers, &q, Access::Read).await?;
    let events: Vec<CacheEvent> = read_json(body).await?;
    if events.len() > MAX_ITEMS {
        return Err(TurboError::BadRequest("too many events"));
    }
    for e in &events {
        match e.event.as_str() {
            "HIT" => s.count_event(&caller.scope, &e.source, true),
            "MISS" => s.count_event(&caller.scope, &e.source, false),
            _ => {}
        }
    }
    Ok(StatusCode::OK.into_response())
}

/// The `/v8` routes.
pub fn router(state: Arc<TurboState>) -> axum::Router {
    axum::Router::new()
        .route("/v8/artifacts/status", get(status))
        .route("/v8/artifacts/events", post(post_events))
        .route(
            "/v8/artifacts/{hash}",
            get(get_artifact)
                .head(get_artifact)
                .put(put_artifact)
                .options(options_artifact),
        )
        .route("/v8/artifacts", post(query_artifacts))
        .with_state(state)
}

/// One registration line in `lib.rs`: mounts the routes on `ext`.
pub fn register(ext: &Extensions, state: Arc<TurboState>) {
    ext.http.register(router(state));
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use asc_rpc::auth::StaticTokens;
    use object_store::memory::InMemory;
    use tower::ServiceExt;

    use super::*;

    const SLUG: &str = "acme.web";

    fn tokens() -> StaticTokens {
        StaticTokens::new()
            .with_token("ci-secret", "ci", Access::Write)
            .with_token("dev-secret", "alice", Access::Write)
            .with_token("ro-secret", "bob", Access::Read)
    }

    fn state() -> (Arc<TurboState>, Arc<InMemory>) {
        let mem = Arc::new(InMemory::new());
        let s = TurboState::new(
            Arc::new(tokens()),
            mem.clone(),
            StorePath::from("root"),
            TurboConfig::default(),
            Arc::new(Metrics::new()),
        )
        .with_trusted_token("ci-secret");
        (Arc::new(s), mem)
    }

    struct Call {
        method: &'static str,
        uri: String,
        token: Option<&'static str>,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    fn call(method: &'static str, uri: &str, token: Option<&'static str>) -> Call {
        Call {
            method,
            uri: uri.to_owned(),
            token,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    impl Call {
        fn header(mut self, k: &'static str, v: impl Into<String>) -> Self {
            self.headers.push((k, v.into()));
            self
        }

        fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
            self.body = b.into();
            self
        }

        async fn send(self, s: &Arc<TurboState>) -> (StatusCode, HeaderMap, Vec<u8>) {
            let mut r = Request::builder().method(self.method).uri(self.uri);
            if let Some(t) = self.token {
                r = r.header("authorization", format!("Bearer {t}"));
            }
            for (k, v) in self.headers {
                r = r.header(k, v);
            }
            let res = router(s.clone())
                .oneshot(r.body(Body::from(self.body)).unwrap())
                .await
                .unwrap();
            let (parts, body) = res.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            (parts.status, parts.headers, bytes.to_vec())
        }
    }

    fn art(hash: &str) -> String {
        format!("/v8/artifacts/{hash}?slug={SLUG}&teamId=team_x")
    }

    async fn put(
        s: &Arc<TurboState>,
        token: &'static str,
        hash: &str,
        data: &[u8],
        pr: Option<u32>,
    ) -> StatusCode {
        let mut c = call("PUT", &art(hash), Some(token))
            .header("x-artifact-tag", "sig-abc")
            .header("x-artifact-duration", "1234")
            .body(data);
        if let Some(n) = pr {
            c = c.header("x-asc-pr", n.to_string());
        }
        c.send(s).await.0
    }

    #[tokio::test]
    async fn status_is_enabled() {
        let (s, _) = state();
        let (st, _, body) = call("GET", "/v8/artifacts/status", None).send(&s).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, br#"{"status":"enabled"}"#);
    }

    #[tokio::test]
    async fn put_then_get_and_head_return_bytes_and_signature() {
        let (s, mem) = state();
        assert_eq!(
            put(&s, "ci-secret", "abc123", b"payload", None).await,
            StatusCode::ACCEPTED
        );
        let (st, h, body) = call("GET", &art("abc123"), Some("ci-secret"))
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, b"payload");
        assert_eq!(h["x-artifact-tag"], "sig-abc");
        assert_eq!(h["x-artifact-duration"], "1234");
        let (st, h, body) = call("HEAD", &art("abc123"), Some("ci-secret"))
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.is_empty());
        assert_eq!(h["content-length"], "7");
        // Stored under the documented key with a meta sibling.
        let meta = mem
            .get(&StorePath::from("root/turbo/acme.web/trusted/abc123.meta"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let meta: Meta = serde_json::from_slice(&meta).unwrap();
        assert_eq!((meta.size, meta.duration), (7, 1234));
        assert_eq!(meta.tag.as_deref(), Some("sig-abc"));
        let (st, _, _) = call("GET", &art("missing"), Some("ci-secret"))
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn upload_without_meta_is_invisible() {
        let (s, mem) = state();
        mem.put(
            &StorePath::from("root/turbo/acme.web/trusted/half"),
            "data".into(),
        )
        .await
        .unwrap();
        let (st, _, _) = call("GET", &art("half"), Some("ci-secret")).send(&s).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn scopes_isolate_prs_and_fall_back_to_trusted() {
        let (s, _) = state();
        assert_eq!(
            put(&s, "ci-secret", "trustedhash", b"T", None).await,
            StatusCode::ACCEPTED
        );
        // A non-trusted token writes to pr-7 only.
        assert_eq!(
            put(&s, "dev-secret", "prhash", b"P", Some(7)).await,
            StatusCode::ACCEPTED
        );
        let get = |token, hash: &'static str, pr: u32| {
            let s = s.clone();
            async move {
                call("GET", &art(hash), Some(token))
                    .header("x-asc-pr", pr.to_string())
                    .send(&s)
                    .await
                    .0
            }
        };
        assert_eq!(get("dev-secret", "prhash", 7).await, StatusCode::OK);
        assert_eq!(get("dev-secret", "prhash", 8).await, StatusCode::NOT_FOUND);
        // Reads fall back to trusted.
        assert_eq!(get("dev-secret", "trustedhash", 8).await, StatusCode::OK);
        // The PR write did not reach trusted.
        let (st, _, _) = call("GET", &art("prhash"), Some("ci-secret"))
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn authorization_is_checked_on_every_endpoint() {
        let (s, _) = state();
        let (st, _, _) = call("GET", &art("h"), None).send(&s).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(
            put(&s, "ro-secret", "h", b"x", None).await,
            StatusCode::FORBIDDEN
        );
        let (st, _, _) = call("GET", &art("h"), Some("nope")).send(&s).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let uri = format!("/v8/artifacts?slug={SLUG}");
        let (st, _, _) = call("POST", &uri, None)
            .body(r#"{"hashes":[]}"#)
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let uri = format!("/v8/artifacts/events?slug={SLUG}");
        let (st, _, _) = call("POST", &uri, None).body("[]").send(&s).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _, _) = call("OPTIONS", &art("h"), None).send(&s).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_bad_slug_hash_and_oversize() {
        let (s, _) = state();
        for uri in [
            "/v8/artifacts/h?slug=noDot",
            "/v8/artifacts/h",
            "/v8/artifacts/h?slug=.b",
            "/v8/artifacts/h?slug=a%2Fb.c",
        ] {
            let (st, _, _) = call("GET", uri, Some("ci-secret")).send(&s).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{uri}");
        }
        let (st, _, _) = call("GET", &art("..%2Fx"), Some("ci-secret"))
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _, _) = call("PUT", &art("big"), Some("ci-secret"))
            .header("content-length", (MAX_ARTIFACT_BYTES + 1).to_string())
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn batch_query_reports_each_hash() {
        let (s, _) = state();
        put(&s, "ci-secret", "aa", b"12345", None).await;
        let uri = format!("/v8/artifacts?slug={SLUG}");
        let (st, _, body) = call("POST", &uri, Some("ci-secret"))
            .body(r#"{"hashes":["aa","bb"]}"#)
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["aa"]["size"], 5);
        assert_eq!(v["aa"]["taskDurationMs"], 1234);
        assert_eq!(v["aa"]["tag"], "sig-abc");
        assert!(v["bb"]["error"].is_object());
        let (st, _, _) = call("POST", &uri, Some("ci-secret"))
            .body(r#"{"hashes":["../x"]}"#)
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_are_counted_as_metrics() {
        let (s, _) = state();
        let uri = format!("/v8/artifacts/events?slug={SLUG}");
        let body = r#"[{"sessionId":"s","source":"REMOTE","event":"HIT","hash":"a","duration":1},
            {"sessionId":"s","source":"REMOTE","event":"MISS","hash":"b"},
            {"sessionId":"s","source":"LOCAL","event":"HIT","hash":"c"},
            {"event":"WHAT"}]"#;
        let (st, _, _) = call("POST", &uri, Some("ci-secret"))
            .body(body)
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::OK);
        let text = s.metrics.render();
        assert!(text.contains("asc_cache_requests_total"), "{text}");
        assert!(text.contains(r#"source="remote""#), "{text}");
        assert!(text.contains(r#"result="miss""#), "{text}");
        assert!(
            text.contains(r#"asc_turbo_hit_ratio{scope="trusted"}"#),
            "{text}"
        );
        let (st, _, _) = call("POST", &uri, Some("ci-secret"))
            .body("not json")
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sweep_applies_per_scope_retention() {
        let (s, mem) = state();
        put(&s, "ci-secret", "old1", b"T", None).await;
        put(&s, "dev-secret", "old2", b"P", Some(3)).await;
        let day = Duration::from_secs(DAY_SECS);
        let now = SystemTime::now();
        // 20 days: the PR entry (14 days) goes, trusted (60 days) stays.
        let stats = s.sweep(now + day * 20).await.unwrap();
        assert_eq!(
            stats,
            SweepStats {
                scanned: 2,
                deleted: 1
            }
        );
        let (st, _, _) = call("GET", &art("old1"), Some("ci-secret")).send(&s).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            mem.head(&StorePath::from("root/turbo/acme.web/pr-3/old2"))
                .await
                .is_err()
        );
        let stats = s.sweep(now + day * 61).await.unwrap();
        assert_eq!(stats.deleted, 1);
        assert!(
            mem.head(&StorePath::from("root/turbo/acme.web/trusted/old1"))
                .await
                .is_err()
        );
    }

    struct FixedScope(Option<Scope>);

    #[async_trait]
    impl ScopeSource for FixedScope {
        async fn scope(&self, token: &BearerToken, _repo: &str) -> Option<Scope> {
            if token.expose() == "ci-secret" {
                self.0.clone()
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn authz_scope_source_drives_scopes() {
        let (s, mem) = state();
        s.set_scope_source(Arc::new(FixedScope(Some(Scope::Pr(42)))))
            .ok()
            .unwrap();
        // The task token writes pr-42 (even though it is a Stage 1 trusted token).
        put(&s, "ci-secret", "h1", b"A", None).await;
        let pr = StorePath::from("root/turbo/acme.web/pr-42/h1");
        assert!(mem.head(&pr).await.is_ok());
        // A developer PAT writes dev-<user>.
        put(&s, "dev-secret", "h2", b"B", None).await;
        let dev = StorePath::from("root/turbo/acme.web/dev-alice/h2");
        assert!(mem.head(&dev).await.is_ok());
        // The PAT cannot see pr-42.
        let (st, _, _) = call("GET", &art("h1"), Some("dev-secret")).send(&s).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[derive(Debug)]
    struct FakeSigner;

    #[async_trait]
    impl Signer for FakeSigner {
        async fn signed_url(
            &self,
            method: Method,
            path: &StorePath,
            _expires_in: Duration,
        ) -> object_store::Result<url::Url> {
            Ok(url::Url::parse(&format!("https://s3.test/{path}?m={method}")).unwrap())
        }
    }

    #[tokio::test]
    async fn options_offers_presigned_urls_for_large_artifacts() {
        let (s, mem) = state();
        let s = Arc::new(
            Arc::into_inner(s)
                .unwrap()
                .with_signer(Arc::new(FakeSigner)),
        );
        let uri = format!("{}&method=put&size={}", art("big"), PRESIGN_MIN_BYTES);
        let (st, _, body) = call("OPTIONS", &uri, Some("ci-secret")).send(&s).await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["put"],
            "https://s3.test/root/turbo/acme.web/trusted/big?m=PUT"
        );
        // Small uploads get no URL.
        let uri = format!("{}&method=put&size=10", art("big"));
        let (_, _, body) = call("OPTIONS", &uri, Some("ci-secret")).send(&s).await;
        assert!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["put"].is_null());
        // The client uploads out of band, then commits the meta.
        let data = vec![7u8; PRESIGN_MIN_BYTES as usize];
        mem.put(
            &StorePath::from("root/turbo/acme.web/trusted/big"),
            data.into(),
        )
        .await
        .unwrap();
        let (st, _, _) = call("PUT", &art("big"), Some("ci-secret"))
            .header("x-asc-presigned", "1")
            .header("x-artifact-tag", "t")
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let (_, _, body) = call("OPTIONS", &art("big"), Some("ci-secret"))
            .send(&s)
            .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["get"],
            "https://s3.test/root/turbo/acme.web/trusted/big?m=GET"
        );
        // Committing a missing upload fails.
        let (st, _, _) = call("PUT", &art("none"), Some("ci-secret"))
            .header("x-asc-presigned", "1")
            .send(&s)
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn trusted_token_file_is_parsed_defensively() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t");
        std::fs::write(&p, "# c\n\n tok-1 \ntok-2\n").unwrap();
        assert_eq!(load_trusted_tokens(&p).unwrap().len(), 2);
        std::fs::write(&p, "bad token\n").unwrap();
        assert!(load_trusted_tokens(&p).is_err());
        assert!(load_trusted_tokens(&dir.path().join("missing")).is_err());
    }
}
