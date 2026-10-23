//! Seed snapshots, server side (SPEC §12.3; PLAN U72).
//!
//! The `seeds` task cuts a seed for each repo's default branch every hour and after 50 landed
//! changesets, uploads it to `seeds/<owner>.<repo>/<changeset>.tar.zst`, points
//! `seeds/<owner>.<repo>/latest` at it and keeps the newest 3. `GET /seeds/<owner>/<repo>/latest`
//! authorises the caller for read access and redirects to a presigned URL, so the bytes never
//! pass through the server. The archive format and the client-side unpack live in
//! [`asc_store::seed`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use asc_rpc::auth::{Access, AuthError, Authorizer};
use asc_store::seed::{self, SeedManifest};
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::signer::Signer;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use tokio::io::AsyncReadExt;

use crate::registry::Extensions;
use crate::turbo::Slug;

/// A seed is cut at least this often.
pub const EVERY: Duration = Duration::from_secs(60 * 60);
/// A seed is also cut once this many changesets landed since the last one.
pub const EVERY_LANDED: u64 = 50;
/// Seeds kept per repo.
pub const KEEP: usize = 3;
/// How long a presigned download URL lives.
const URL_TTL: Duration = Duration::from_secs(15 * 60);
/// The task wakes this often to check what is due.
const TICK: Duration = Duration::from_secs(60);
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;
const MAX_POINTER_BYTES: u64 = 128;

/// Why a seed could not be cut or published.
#[derive(Debug, thiserror::Error)]
pub enum SeedsError {
    #[error("seed source: {0}")]
    Source(String),
    #[error(transparent)]
    Seed(#[from] seed::SeedError),
    #[error("store: {0}")]
    Store(#[from] object_store::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// What to put in the next seed of one repo.
#[derive(Clone, Debug)]
pub struct SeedPlan {
    /// Lowercase hex id of the default branch's tip changeset.
    pub changeset: String,
    /// Landed changesets so far (monotonic); the 50-changeset trigger compares it.
    pub landed: u64,
    /// The cache directory the files are read from.
    pub cache: PathBuf,
    /// Cache-relative files: the tip's history metadata within the default window, all trees,
    /// the claimed manifests (G07) and the `.asc/prefetch` blobs.
    pub files: Vec<String>,
}

/// Supplies the repos to seed and what goes into each seed. The server wiring implements it
/// over the registry, the history window and G07's claims.
#[async_trait]
pub trait SeedSource: Send + Sync {
    /// Repos whose default branch gets seeds, as `owner.repo` slugs.
    async fn repos(&self) -> Vec<Slug>;
    async fn plan(&self, repo: &Slug) -> Result<SeedPlan, SeedsError>;
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn repo_dir(slug: &Slug) -> String {
    slug.repo_name().replace('/', ".")
}

fn hex_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// -------------------------------------------------------------------------------- storage

/// The seed objects of one bucket.
pub struct SeedStore {
    store: Arc<dyn ObjectStore>,
    root: StorePath,
    signer: Option<Arc<dyn Signer>>,
    keep: usize,
}

impl SeedStore {
    pub fn new(store: Arc<dyn ObjectStore>, root: StorePath) -> Self {
        Self {
            store,
            root,
            signer: None,
            keep: KEEP,
        }
    }

    /// The presigner used for the redirect (S3).
    #[must_use]
    pub fn with_signer(mut self, signer: Arc<dyn Signer>) -> Self {
        self.signer = Some(signer);
        self
    }

    fn dir(&self, slug: &Slug) -> StorePath {
        self.root.clone().join("seeds").join(repo_dir(slug))
    }

    fn object(&self, slug: &Slug, changeset: &str) -> StorePath {
        self.dir(slug).join(format!("{changeset}.tar.zst"))
    }

    fn pointer(&self, slug: &Slug) -> StorePath {
        self.dir(slug).join("latest")
    }

    /// The changeset `latest` names, if there is a valid pointer.
    pub async fn latest(&self, slug: &Slug) -> Result<Option<String>, SeedsError> {
        let got = match self.store.get(&self.pointer(slug)).await {
            Ok(g) => g,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if got.meta.size > MAX_POINTER_BYTES {
            return Ok(None);
        }
        let bytes = got.bytes().await?;
        Ok(std::str::from_utf8(&bytes)
            .ok()
            .map(str::trim)
            .filter(|s| hex_ok(s))
            .map(str::to_owned))
    }

    /// Uploads the seed file, then moves `latest`, then prunes to the newest [`KEEP`].
    pub async fn publish(
        &self,
        slug: &Slug,
        changeset: &str,
        file: &std::path::Path,
    ) -> Result<(), SeedsError> {
        if !hex_ok(changeset) {
            return Err(SeedsError::Source("bad changeset id".into()));
        }
        let path = self.object(slug, changeset);
        let upload = self.store.put_multipart(&path).await?;
        let mut w = WriteMultipart::new_with_chunk_size(upload, UPLOAD_CHUNK);
        let res: Result<(), SeedsError> = async {
            let mut f = tokio::fs::File::open(file).await?;
            let mut buf = vec![0u8; 1024 * 1024];
            loop {
                let n = f.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                w.wait_for_capacity(2).await?;
                w.write(&buf[..n]);
            }
        }
        .await;
        match res {
            Ok(()) => w.finish().await.map(drop)?,
            Err(e) => {
                let _ = w.abort().await;
                return Err(e);
            }
        }
        self.store
            .put(
                &self.pointer(slug),
                PutPayload::from(changeset.as_bytes().to_vec()),
            )
            .await?;
        self.prune(slug, changeset).await
    }

    async fn prune(&self, slug: &Slug, current: &str) -> Result<(), SeedsError> {
        let dir = self.dir(slug);
        let mut listing = self.store.list(Some(&dir));
        let mut seeds = Vec::new();
        while let Some(m) = listing.next().await {
            let m = m?;
            let Some(name) = m.location.filename() else {
                continue;
            };
            if let Some(id) = name.strip_suffix(".tar.zst")
                && hex_ok(id)
            {
                seeds.push((m.last_modified, id.to_owned(), m.location.clone()));
            }
        }
        seeds.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let mut kept = 0;
        for (_, id, loc) in seeds {
            if id == current || kept < self.keep {
                kept += 1;
                continue;
            }
            match self.store.delete(&loc).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Changesets with a seed object, newest first.
    pub async fn list(&self, slug: &Slug) -> Result<Vec<String>, SeedsError> {
        let mut listing = self.store.list(Some(&self.dir(slug)));
        let mut seeds = Vec::new();
        while let Some(m) = listing.next().await {
            let m = m?;
            if let Some(id) = m
                .location
                .filename()
                .and_then(|n| n.strip_suffix(".tar.zst"))
                .filter(|id| hex_ok(id))
            {
                seeds.push((m.last_modified, id.to_owned()));
            }
        }
        seeds.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        Ok(seeds.into_iter().map(|(_, id)| id).collect())
    }
}

// ----------------------------------------------------------------------------------- task

#[derive(Clone, Copy, Debug)]
struct Last {
    at: u64,
    landed: u64,
}

/// Decides whether a seed is due: first time, an hour old, or [`EVERY_LANDED`] landed since.
/// An unchanged tip never republishes.
fn due(last: Option<(Last, &str)>, plan: &SeedPlan, now: u64) -> bool {
    match last {
        None => true,
        Some((_, tip)) if tip == plan.changeset => false,
        Some((l, _)) => {
            now.saturating_sub(l.at) >= EVERY.as_secs()
                || plan.landed.saturating_sub(l.landed) >= EVERY_LANDED
        }
    }
}

/// The `seeds` task.
pub struct SeedTask {
    source: Arc<dyn SeedSource>,
    store: Arc<SeedStore>,
    tmp: PathBuf,
    last: Mutex<HashMap<String, (Last, String)>>,
}

impl SeedTask {
    /// `tmp` is where seed files are staged before upload.
    pub fn new(source: Arc<dyn SeedSource>, store: Arc<SeedStore>, tmp: PathBuf) -> Self {
        Self {
            source,
            store,
            tmp,
            last: Mutex::default(),
        }
    }

    /// Cuts and publishes a seed for `slug` now, whatever the schedule says.
    pub async fn publish_now(&self, slug: &Slug, now: u64) -> Result<(), SeedsError> {
        let plan = self.source.plan(slug).await?;
        self.publish_plan(slug, &plan, now).await
    }

    async fn publish_plan(&self, slug: &Slug, plan: &SeedPlan, now: u64) -> Result<(), SeedsError> {
        tokio::fs::create_dir_all(&self.tmp).await?;
        let file = self
            .tmp
            .join(format!("{}-{}.tmp", repo_dir(slug), plan.changeset));
        let manifest = SeedManifest::new(&slug.repo_name(), &plan.changeset, now);
        let (cache, files, out) = (plan.cache.clone(), plan.files.clone(), file.clone());
        let packed = tokio::task::spawn_blocking(move || {
            let f = std::fs::File::create(&out)?;
            let mut w = std::io::BufWriter::new(f);
            let stats = seed::pack(&cache, &manifest, &files, &mut w)?;
            std::io::Write::flush(&mut w)?;
            Ok::<_, SeedsError>(stats)
        })
        .await
        .map_err(|e| SeedsError::Source(e.to_string()))?;
        let result = match packed {
            Ok(_) => self.store.publish(slug, &plan.changeset, &file).await,
            Err(e) => Err(e),
        };
        let _ = tokio::fs::remove_file(&file).await;
        result?;
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                repo_dir(slug),
                (
                    Last {
                        at: now,
                        landed: plan.landed,
                    },
                    plan.changeset.clone(),
                ),
            );
        Ok(())
    }

    /// One pass over every repo: publishes the seeds that are due. A failing repo is logged and
    /// does not stop the others. Returns the repos published.
    pub async fn tick(&self, now: u64) -> Vec<String> {
        let mut done = Vec::new();
        for slug in self.source.repos().await {
            let plan = match self.source.plan(&slug).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(event = "seeds.plan", repo = %slug.repo_name(), error = %e);
                    continue;
                }
            };
            let go = {
                let g = self.last.lock().unwrap_or_else(PoisonError::into_inner);
                due(
                    g.get(&repo_dir(&slug)).map(|(l, t)| (*l, t.as_str())),
                    &plan,
                    now,
                )
            };
            if !go {
                continue;
            }
            match self.publish_plan(&slug, &plan, now).await {
                Ok(()) => done.push(slug.repo_name()),
                Err(e) => {
                    tracing::warn!(event = "seeds.publish", repo = %slug.repo_name(), error = %e);
                }
            }
        }
        done
    }

    /// Runs [`tick`](Self::tick) every minute until the handle is aborted.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut t = tokio::time::interval(TICK);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                t.tick().await;
                self.tick(unix_now()).await;
            }
        })
    }
}

// ------------------------------------------------------------------------------------ http

/// Shared state of the `/seeds` route.
pub struct SeedsState {
    authorizer: Arc<dyn Authorizer>,
    store: Arc<SeedStore>,
}

impl SeedsState {
    pub fn new(authorizer: Arc<dyn Authorizer>, store: Arc<SeedStore>) -> Self {
        Self { authorizer, store }
    }
}

enum HttpError {
    BadRequest(&'static str),
    Auth(AuthError),
    NotFound,
    NoPresign,
    Internal(String),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Auth(AuthError::Unauthenticated) => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            Self::Auth(AuthError::Malformed) => (StatusCode::BAD_REQUEST, "malformed credentials"),
            Self::Auth(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Self::NotFound => (StatusCode::NOT_FOUND, "no seed"),
            Self::NoPresign => (StatusCode::NOT_IMPLEMENTED, "seeds need a presigning store"),
            Self::Internal(e) => {
                tracing::warn!(event = "seeds.error", error = %e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };
        (
            status,
            axum::Json(serde_json::json!({ "error": { "message": msg } })),
        )
            .into_response()
    }
}

async fn latest(
    State(s): State<Arc<SeedsState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    // An owner with a dot would alias another repo's `owner.repo` directory.
    if owner.contains('.') {
        return Err(HttpError::BadRequest("bad owner or repo"));
    }
    let slug = Slug::parse(&format!("{owner}.{repo}"))
        .map_err(|_| HttpError::BadRequest("bad owner or repo"))?;
    let credential = crate::authn::credential(&headers).map_err(HttpError::Auth)?;
    s.authorizer
        .authorize_credential(credential.as_ref(), &slug.repo_name(), Access::Read)
        .await
        .map_err(HttpError::Auth)?;
    let changeset = s
        .store
        .latest(&slug)
        .await
        .map_err(|e| HttpError::Internal(e.to_string()))?
        .ok_or(HttpError::NotFound)?;
    let signer = s.store.signer.as_ref().ok_or(HttpError::NoPresign)?;
    let url = signer
        .signed_url(Method::GET, &s.store.object(&slug, &changeset), URL_TTL)
        .await
        .map_err(|e| HttpError::Internal(e.to_string()))?;
    let location =
        HeaderValue::from_str(url.as_str()).map_err(|e| HttpError::Internal(e.to_string()))?;
    Ok((
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            ),
        ],
    )
        .into_response())
}

/// The `/seeds/<owner>/<repo>/latest` route.
pub fn router(state: Arc<SeedsState>) -> axum::Router {
    let routes = axum::Router::new()
        .route("/seeds/{owner}/{repo}/latest", get(latest))
        .with_state(state);
    // The edge answers 401 with the Basic challenge first, and 400 for a malformed header.
    crate::authn::guard(routes, crate::authn::Surface::Api)
}

/// One registration line in `lib.rs`.
pub fn register(ext: &Extensions, state: Arc<SeedsState>) {
    ext.http.register(router(state));
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use asc_core::id::FileId;
    use asc_rpc::auth::StaticTokens;
    use asc_store::cachedir::CacheDir;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;
    use object_store::memory::InMemory;
    use tower::ServiceExt;

    use super::*;

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

    fn slug() -> Slug {
        Slug::parse("acme.web").unwrap()
    }

    fn tip(n: u32) -> String {
        format!("{n:02x}").repeat(32)
    }

    struct Fixed {
        cache: PathBuf,
        files: Vec<String>,
        state: Mutex<(String, u64)>,
    }

    #[async_trait]
    impl SeedSource for Fixed {
        async fn repos(&self) -> Vec<Slug> {
            vec![slug()]
        }
        async fn plan(&self, _: &Slug) -> Result<SeedPlan, SeedsError> {
            let g = self.state.lock().unwrap();
            Ok(SeedPlan {
                changeset: g.0.clone(),
                landed: g.1,
                cache: self.cache.clone(),
                files: self.files.clone(),
            })
        }
    }

    async fn fixture(
        tmp: &std::path::Path,
    ) -> (Arc<Fixed>, Arc<InMemory>, Arc<SeedStore>, Arc<SeedTask>) {
        let cache = CacheDir::open(tmp.join("cache"), tmp.join("ctmp")).unwrap();
        let key = ObjKey::File(FileId::of(b"hot"));
        cache.put(&key, encode_stored(b"hot")).await.unwrap();
        let src = Arc::new(Fixed {
            cache: tmp.join("cache"),
            files: vec![key.path()],
            state: Mutex::new((tip(1), 0)),
        });
        let mem = Arc::new(InMemory::new());
        let store = Arc::new(
            SeedStore::new(mem.clone(), StorePath::from("root")).with_signer(Arc::new(FakeSigner)),
        );
        let task = Arc::new(SeedTask::new(src.clone(), store.clone(), tmp.join("stage")));
        (src, mem, store, task)
    }

    #[tokio::test]
    async fn schedule_hourly_or_fifty_landed_and_keeps_three() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, _mem, store, task) = fixture(tmp.path()).await;
        let t0 = 1_000_000;
        assert_eq!(task.tick(t0).await, ["acme/web"]);
        // Same tip: never again.
        assert!(task.tick(t0 + 2 * 3600).await.is_empty());
        // New tip, 10 minutes later, 49 landed: not due.
        *src.state.lock().unwrap() = (tip(2), 49);
        assert!(task.tick(t0 + 600).await.is_empty());
        // 50 landed: due.
        *src.state.lock().unwrap() = (tip(2), 50);
        assert_eq!(task.tick(t0 + 600).await.len(), 1);
        // New tip, 1 landed, an hour later: due.
        *src.state.lock().unwrap() = (tip(3), 51);
        assert!(task.tick(t0 + 700).await.is_empty());
        assert_eq!(task.tick(t0 + 600 + 3600).await.len(), 1);
        // A fourth publish prunes the oldest.
        *src.state.lock().unwrap() = (tip(4), 200);
        assert_eq!(task.tick(t0 + 5000).await.len(), 1);
        let kept = store.list(&slug()).await.unwrap();
        assert_eq!(kept.len(), KEEP, "{kept:?}");
        assert!(kept.contains(&tip(4)) && !kept.contains(&tip(1)));
        assert_eq!(store.latest(&slug()).await.unwrap().unwrap(), tip(4));
    }

    #[tokio::test]
    async fn published_seed_unpacks_and_is_verified() {
        let tmp = tempfile::tempdir().unwrap();
        let (_src, mem, _store, task) = fixture(tmp.path()).await;
        task.publish_now(&slug(), 5).await.unwrap();
        let obj = mem
            .get(&StorePath::from(format!(
                "root/seeds/acme.web/{}.tar.zst",
                tip(1)
            )))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let got =
            seed::unpack_reader(&obj[..], &tmp.path().join("client"), Some("acme/web")).unwrap();
        assert_eq!((got.installed, got.manifest.created), (1, 5));
        assert_eq!(got.manifest.changeset, tip(1));
        // A flipped byte is rejected on read.
        let mut bad = obj.to_vec();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        assert!(seed::unpack_reader(&bad[..], &tmp.path().join("c2"), None).is_err());
        assert!(
            std::fs::read_dir(tmp.path().join("stage"))
                .unwrap()
                .next()
                .is_none(),
            "staging files are removed"
        );
    }

    async fn get(
        app: &axum::Router,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, Option<String>) {
        let mut req = Request::get(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let loc = res
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        (res.status(), loc)
    }

    #[tokio::test]
    async fn latest_redirects_only_for_authorised_readers() {
        let tmp = tempfile::tempdir().unwrap();
        let (_src, _mem, store, task) = fixture(tmp.path()).await;
        let tokens = StaticTokens::new()
            .with_token("rd", "bob", Access::Read)
            .with_token("rw", "al", Access::Write);
        let app = router(Arc::new(SeedsState::new(Arc::new(tokens), store.clone())));
        let uri = "/seeds/acme/web/latest";
        // Nothing published yet.
        assert_eq!(get(&app, uri, Some("rd")).await.0, StatusCode::NOT_FOUND);
        task.publish_now(&slug(), 1).await.unwrap();
        // Anonymous and unknown tokens are refused.
        assert_eq!(get(&app, uri, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            get(&app, uri, Some("nope")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (st, loc) = get(&app, uri, Some("rd")).await;
        assert_eq!(st, StatusCode::FOUND);
        assert_eq!(
            loc.unwrap(),
            format!(
                "https://s3.test/root/seeds/acme.web/{}.tar.zst?m=GET",
                tip(1)
            )
        );
        // Traversal-looking names never reach the store.
        assert_eq!(
            get(&app, "/seeds/acme/..%2Fweb/latest", Some("rd")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&app, "/seeds/ac.me/web/latest", Some("rd")).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn pointer_is_validated_and_signer_is_required() {
        let tmp = tempfile::tempdir().unwrap();
        let (_src, mem, _store, _task) = fixture(tmp.path()).await;
        let plain = Arc::new(SeedStore::new(mem.clone(), StorePath::from("root")));
        mem.put(
            &StorePath::from("root/seeds/acme.web/latest"),
            PutPayload::from(&b"../../etc/passwd"[..]),
        )
        .await
        .unwrap();
        assert!(plain.latest(&slug()).await.unwrap().is_none());
        mem.put(
            &StorePath::from("root/seeds/acme.web/latest"),
            PutPayload::from(tip(1).into_bytes()),
        )
        .await
        .unwrap();
        let tokens = StaticTokens::new().with_token("rd", "bob", Access::Read);
        let app = router(Arc::new(SeedsState::new(Arc::new(tokens), plain)));
        assert_eq!(
            get(&app, "/seeds/acme/web/latest", Some("rd")).await.0,
            StatusCode::NOT_IMPLEMENTED
        );
    }

    #[test]
    fn due_rules() {
        let plan = |t: &str, landed| SeedPlan {
            changeset: t.into(),
            landed,
            cache: PathBuf::new(),
            files: vec![],
        };
        let l = Last { at: 0, landed: 0 };
        assert!(due(None, &plan("aa", 0), 0));
        assert!(!due(Some((l, "aa")), &plan("aa", 500), 99_999));
        assert!(!due(Some((l, "aa")), &plan("bb", 49), 3599));
        assert!(due(Some((l, "aa")), &plan("bb", 50), 10));
        assert!(due(Some((l, "aa")), &plan("bb", 1), 3600));
    }
}
