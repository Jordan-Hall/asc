//! The build cache fronts (docs/designs/build-cache.md §4.3): the cache protocols of turbo, Nx,
//! Bazel, Gradle, Maven and ccache on the loopback interface, answered from the shared store.
//! The daemon serves them while the front file exists; `asc cache serve` serves them in the
//! foreground (CI, or a machine without the daemon).
//!
//! The front file (`<ASC_HOME>/buildcache-front.json`) holds the port, the token and the store;
//! `asc cache setup <tool>` creates it, and the configuration it writes for each tool names
//! that port and token. Every request needs the token (Bearer, or Basic with the token as the
//! password) and a loopback `Host`, so neither another user's process without the file nor a
//! web page that rebinds a name to 127.0.0.1 can read or write entries.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use asc_buildcache::{Action, ActionKey, Output, OutputKind, Store, hex, unhex};
use asc_state::AscHome;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::remote::MAX_ARTIFACT;
use super::upstream::{self, Upstream};

/// The front file's name in the asc home.
const FILE: &str = "buildcache-front.json";
/// Turbo's artefact signature header, kept with the artefact.
const TAG: &str = "x-artifact-tag";

/// Where the fronts listen and with what token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontFile {
    pub port: u16,
    pub token: String,
    /// The store the fronts answer from.
    pub store: PathBuf,
}

impl FrontFile {
    pub fn path(home: &AscHome) -> PathBuf {
        home.root().join(FILE)
    }

    /// The front file, if there is one that parses.
    pub fn load(home: &AscHome) -> Option<FrontFile> {
        let text = std::fs::read(Self::path(home)).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// The front file, created with a free port and a new token when absent; an existing one
    /// is kept (the tools' configuration names its port and token) with `store` updated.
    pub fn ensure(home: &AscHome, store: &Path) -> std::io::Result<FrontFile> {
        let store = std::path::absolute(store)?;
        if let Some(mut f) = Self::load(home) {
            if f.store != store {
                f.store = store;
                f.write(home)?;
            }
            return Ok(f);
        }
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let f = FrontFile { port, token, store };
        f.write(home)?;
        Ok(f)
    }

    fn write(&self, home: &AscHome) -> std::io::Result<()> {
        let path = Self::path(home);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)
    }

    /// The base URL of the fronts.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }
}

#[derive(Clone)]
struct Front {
    store: Store,
    token: Arc<str>,
    /// Queue what is stored for `asc cache push` (the server tier in read-write mode).
    queue: bool,
    /// The daemon's server tier for turbo, per team.
    upstream: Option<Arc<dyn Upstream>>,
}

/// The routes of every front over `store`.
pub(crate) fn router(
    store: Store,
    token: &str,
    queue: bool,
    upstream: Option<Arc<dyn Upstream>>,
) -> Router {
    let front = Front {
        store,
        token: Arc::from(token),
        queue,
        upstream,
    };
    Router::new()
        .route("/v8/artifacts/status", get(turbo_status))
        .route("/v8/artifacts/events", post(no_content))
        .route("/v8/artifacts/{hash}", any(turbo))
        .route("/v1/cache/{hash}", any(nx))
        .route("/bazel/{kind}/{hash}", any(bazel))
        .route("/gradle/{key}", any(gradle))
        .route("/maven/{*path}", any(maven))
        .route("/ccache/{*path}", any(ccache))
        .layer(DefaultBodyLimit::max(MAX_ARTIFACT))
        .layer(middleware::from_fn_with_state(front.clone(), guard))
        .with_state(front)
}

/// Serves `router` on `listener` until `stop` resolves.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    stop: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(stop)
        .await
}

pub async fn bind(f: &FrontFile) -> std::io::Result<TcpListener> {
    TcpListener::bind(f.addr()).await
}

fn host_is_local(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(n, _)| n),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether the request carries the token.
fn authorised(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let (scheme, cred) = value.split_once(' ').unwrap_or((value, ""));
    let cred = cred.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return same(cred.as_bytes(), token.as_bytes());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let Ok(raw) = STANDARD.decode(cred) else {
            return false;
        };
        let password = raw
            .iter()
            .position(|b| *b == b':')
            .map_or(&raw[..0], |i| &raw[i + 1..]);
        return same(password, token.as_bytes());
    }
    false
}

async fn guard(State(front): State<Front>, req: Request, next: Next) -> Response {
    let hosts: Vec<&HeaderValue> = req.headers().get_all(header::HOST).iter().collect();
    let local = match hosts.as_slice() {
        [h] => h.to_str().is_ok_and(host_is_local),
        // HTTP/2 carries the authority in the URI.
        [] => req.uri().host().is_some_and(host_is_local),
        _ => false,
    };
    if !local {
        return (StatusCode::FORBIDDEN, "not a loopback host").into_response();
    }
    if !authorised(req.headers(), &front.token) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"asc build cache\"")],
            "the build cache token is missing or wrong",
        )
            .into_response();
    }
    next.run(req).await
}

async fn turbo_status() -> Response {
    axum::Json(serde_json::json!({ "status": "enabled" })).into_response()
}

async fn no_content() -> StatusCode {
    StatusCode::OK
}

fn key(tool: &str, kind: &str, name: &str) -> ActionKey {
    ActionKey::builder(tool)
        .part("kind", kind.as_bytes())
        .part("key", name.as_bytes())
        .finish()
}

/// A tool's key: printable, no `..`, at most 1 KiB.
fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1024
        && name
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
        && name.bytes().all(|b| b.is_ascii_graphic())
}

/// The body of an entry and its turbo tag, if the entry and its blobs are there.
fn body_of(store: &Store, action: &Action) -> Option<(PathBuf, u64, Option<Vec<u8>>)> {
    let body = action.outputs.iter().find(|o| o.path == "body")?;
    let id = unhex(&body.blob)?;
    // Streamed from a plain file: a blob kept compressed is stored plain from its first hit.
    let path = store.plain_blob_path(&id).ok()??;
    let tag = action
        .outputs
        .iter()
        .find(|o| o.path == "tag")
        .and_then(|o| unhex(&o.blob))
        .and_then(|id| store.read_blob(&id).ok());
    Some((path, body.size, tag))
}

async fn file_body(path: &Path) -> std::io::Result<Body> {
    let file = tokio::fs::File::open(path).await?;
    Ok(Body::from_stream(tokio_util::io::ReaderStream::new(file)))
}

/// Writes the request body to a file in the store's `tmp`, then stores it as a blob. `Ok(None)`
/// when it is over [`MAX_ARTIFACT`]; the sha256 of the bytes is returned for Bazel's check.
async fn take_body(
    store: &Store,
    body: Body,
) -> std::io::Result<Option<([u8; 32], u64, [u8; 32])>> {
    let dir = store.root().join("tmp");
    tokio::fs::create_dir_all(&dir).await?;
    let tmp = dir.join(format!("front-{}", uuid::Uuid::new_v4().simple()));
    let mut out = tokio::fs::File::create(&tmp).await?;
    let mut sha = sha2::Sha256::new();
    let mut size = 0usize;
    let mut stream = body.into_data_stream();
    let copied: std::io::Result<bool> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            size += chunk.len();
            if size > MAX_ARTIFACT {
                return Ok(false);
            }
            sha.update(&chunk);
            out.write_all(&chunk).await?;
        }
        out.flush().await?;
        Ok(true)
    }
    .await;
    drop(out);
    let stored = match copied {
        Ok(true) => {
            let (store, path) = (store.clone(), tmp.clone());
            tokio::task::spawn_blocking(move || {
                let mut f = std::fs::File::open(&path)?;
                store.put_blob_reader(&mut f)
            })
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r)
            .map(Some)
        }
        Ok(false) => Ok(None),
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(stored?.map(|(id, n)| (id, n, sha.finalize().into())))
}

/// What a `PUT` may do to an entry that is already there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Existing {
    Replace,
    /// Nx: a record is never overwritten (409).
    Conflict,
}

struct Kv<'a> {
    tool: &'a str,
    kind: &'a str,
    name: &'a str,
    existing: Existing,
    /// Bazel CAS: the body's sha256 must be the key.
    sha256: bool,
}

fn status(code: StatusCode) -> Response {
    code.into_response()
}

async fn kv(
    front: &Front,
    kv: Kv<'_>,
    method: &Method,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    if !valid(kv.name) {
        return status(StatusCode::BAD_REQUEST);
    }
    let key = key(kv.tool, kv.kind, kv.name);
    let store = &front.store;
    match *method {
        Method::GET | Method::HEAD => {
            let origin = asc_buildcache::Origin::default();
            let found = if *method == Method::GET {
                store.lookup(kv.tool, &key, &origin)
            } else {
                store.get_action(&key)
            };
            let Some((path, size, tag)) = found.ok().flatten().and_then(|a| body_of(store, &a))
            else {
                return status(StatusCode::NOT_FOUND);
            };
            let mut res = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, size);
            if let Some(tag) = tag.and_then(|t| HeaderValue::from_bytes(&t).ok()) {
                res = res.header(TAG, tag);
            }
            let body = if *method == Method::HEAD {
                Body::empty()
            } else {
                match file_body(&path).await {
                    Ok(b) => b,
                    Err(_) => return status(StatusCode::NOT_FOUND),
                }
            };
            res.body(body)
                .unwrap_or_else(|_| status(StatusCode::INTERNAL_SERVER_ERROR))
        }
        Method::PUT | Method::POST => {
            if kv.existing == Existing::Conflict && store.get_action(&key).ok().flatten().is_some()
            {
                return status(StatusCode::CONFLICT);
            }
            let (blob, size, sha) = match take_body(store, body).await {
                Ok(Some(t)) => t,
                Ok(None) => return status(StatusCode::PAYLOAD_TOO_LARGE),
                Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
            };
            if kv.sha256 && hex(&sha) != kv.name.to_ascii_lowercase() {
                return (StatusCode::BAD_REQUEST, "the body is not its sha256").into_response();
            }
            let mut action = Action::new(kv.tool, kv.name, asc_buildcache::Origin::default());
            action.outputs.push(Output {
                path: "body".into(),
                kind: OutputKind::File,
                size,
                blob: hex(&blob),
            });
            if let Some(tag) = headers.get(TAG)
                && let Ok((id, n)) = store.put_blob(tag.as_bytes())
            {
                action.outputs.push(Output {
                    path: "tag".into(),
                    kind: OutputKind::File,
                    size: n,
                    blob: hex(&id),
                });
            }
            if store.put_action(&key, &action).is_err() {
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            }
            if front.queue {
                let _ = store.queue_push(&key);
            }
            status(StatusCode::OK)
        }
        Method::DELETE => match store.remove(&key) {
            Ok(true) => status(StatusCode::OK),
            Ok(false) => status(StatusCode::NOT_FOUND),
            Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
        },
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    }
}

/// The team turbo names in the query (`slug`, or `teamId`), when it is a work tree's.
fn team(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == "slug" || k == "teamId")
        .map(|(_, v)| v.into_owned())
        .filter(|t| upstream::is_team(t))
}

async fn turbo(
    State(f): State<Front>,
    UrlPath(hash): UrlPath<String>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let up = f
        .upstream
        .clone()
        .zip(team(query.as_deref()))
        .filter(|_| valid(&hash));
    let artifact = key("turbo", "artifact", &hash);
    // A local miss is fetched from the team's server first, so the answer below is a hit.
    if let Some((up, team)) = &up
        && matches!(method, Method::GET | Method::HEAD)
        && f.store.get_action(&artifact).ok().flatten().is_none()
    {
        up.fetch(team, &f.store, &artifact).await;
    }
    let res = kv(
        &f,
        Kv {
            tool: "turbo",
            kind: "artifact",
            name: &hash,
            existing: Existing::Replace,
            sha256: false,
        },
        &method,
        &headers,
        body,
    )
    .await;
    // Turbo reads a JSON reply to an upload.
    if method == Method::PUT && res.status() == StatusCode::OK {
        if let Some((up, team)) = up {
            up.send(team, f.store.clone(), artifact);
        }
        return axum::Json(serde_json::json!({ "urls": [] })).into_response();
    }
    res
}

async fn nx(
    State(f): State<Front>,
    UrlPath(hash): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if method == Method::PUT && !headers.contains_key(header::CONTENT_LENGTH) {
        return status(StatusCode::LENGTH_REQUIRED);
    }
    kv(
        &f,
        Kv {
            tool: "nx",
            kind: "task",
            name: &hash,
            existing: Existing::Conflict,
            sha256: false,
        },
        &method,
        &headers,
        body,
    )
    .await
}

async fn bazel(
    State(f): State<Front>,
    UrlPath((kind, hash)): UrlPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let kind = match kind.as_str() {
        "ac" => "ac",
        "cas" => "cas",
        _ => return status(StatusCode::NOT_FOUND),
    };
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return status(StatusCode::BAD_REQUEST);
    }
    kv(
        &f,
        Kv {
            tool: "bazel",
            kind,
            name: &hash.to_ascii_lowercase(),
            existing: Existing::Replace,
            sha256: kind == "cas",
        },
        &method,
        &headers,
        body,
    )
    .await
}

async fn gradle(
    State(f): State<Front>,
    UrlPath(k): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    kv(
        &f,
        Kv {
            tool: "gradle",
            kind: "entry",
            name: &k,
            existing: Existing::Replace,
            sha256: false,
        },
        &method,
        &headers,
        body,
    )
    .await
}

async fn maven(
    State(f): State<Front>,
    UrlPath(path): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    kv(
        &f,
        Kv {
            tool: "maven",
            kind: "file",
            name: &path,
            existing: Existing::Replace,
            sha256: false,
        },
        &method,
        &headers,
        body,
    )
    .await
}

async fn ccache(
    State(f): State<Front>,
    UrlPath(path): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // ccache's layouts (`flat`, `subdirs`, `bazel`) all end in the key: one entry whatever
    // the layout.
    let name = path.rsplit('/').next().unwrap_or_default().to_owned();
    kv(
        &f,
        Kv {
            tool: "ccache",
            kind: "result",
            name: &name,
            existing: Existing::Replace,
            sha256: false,
        },
        &method,
        &headers,
        body,
    )
    .await
}

/// Serves the fronts the front file names, following it: started when it appears, restarted
/// when its port, token or store change, stopped when it goes. The daemon runs this.
pub async fn supervise(home: AscHome, mut stop: tokio::sync::watch::Receiver<bool>) {
    let tier: Arc<dyn Upstream> = Arc::new(upstream::ServerTier::new(home.clone()));
    let mut current: Option<(FrontFile, tokio::sync::oneshot::Sender<()>)> = None;
    loop {
        let wanted = FrontFile::load(&home);
        if current.as_ref().map(|(f, _)| f) != wanted.as_ref() {
            if let Some((_, tx)) = current.take() {
                let _ = tx.send(());
            }
            if let Some(f) = wanted
                && let Ok(store) = Store::open(&f.store)
                && let Ok(listener) = bind(&f).await
            {
                let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                let app = router(store, &f.token, false, Some(tier.clone()));
                tokio::spawn(serve(listener, app, async move {
                    let _ = rx.await;
                }));
                current = Some((f, tx));
            }
        }
        tokio::select! {
            _ = stop.changed() => break,
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }
    }
    if let Some((_, tx)) = current {
        let _ = tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user: &str, pass: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let v = format!("Basic {}", STANDARD.encode(format!("{user}:{pass}")));
        h.insert(header::AUTHORIZATION, v.parse().unwrap());
        h
    }

    #[test]
    fn a_compressed_body_is_streamed_from_a_plain_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s"))
            .unwrap()
            .with_compress(asc_buildcache::Compress::Always);
        let bytes = b"artifact ".repeat(2000);
        let (id, size) = store.put_blob(&bytes).unwrap();
        assert!(store.blob_file(&id).unwrap().1);
        let mut action = Action::new("turbo", "x", asc_buildcache::Origin::default());
        action.outputs.push(asc_buildcache::Output {
            path: "body".into(),
            kind: asc_buildcache::OutputKind::File,
            size,
            blob: asc_buildcache::hex(&id),
        });
        let (path, len, tag) = body_of(&store, &action).unwrap();
        assert_eq!((len, tag), (bytes.len() as u64, None));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!store.blob_file(&id).unwrap().1);
    }

    #[test]
    fn the_token_is_accepted_as_bearer_or_basic_password_only() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer t0k".parse().unwrap());
        assert!(authorised(&h, "t0k"));
        assert!(!authorised(&h, "t0kx"));
        assert!(authorised(&basic("asc", "t0k"), "t0k"));
        assert!(authorised(&basic("", "t0k"), "t0k"));
        assert!(!authorised(&basic("t0k", ""), "t0k"));
        assert!(!authorised(&HeaderMap::new(), "t0k"));
    }

    #[test]
    fn only_loopback_hosts_and_safe_keys_pass() {
        for ok in ["127.0.0.1:4000", "localhost:1", "[::1]:9", "LOCALHOST"] {
            assert!(host_is_local(ok), "{ok}");
        }
        for bad in ["127.0.0.1.evil.example:1", "example.org", "10.0.0.1:80"] {
            assert!(!host_is_local(bad), "{bad}");
        }
        assert!(valid("ab/cd/ef"));
        for bad in ["", "a/../b", "a//b", "./a", "a b", "a\u{7f}"] {
            assert!(!valid(bad), "{bad:?}");
        }
    }

    /// A server tier that holds one artefact and records what it is asked.
    struct Fake {
        held: (ActionKey, Vec<u8>),
        fetched: std::sync::Mutex<Vec<String>>,
        sent: tokio::sync::mpsc::UnboundedSender<(String, ActionKey)>,
    }

    impl Upstream for Fake {
        fn fetch<'a>(
            &'a self,
            team: &'a str,
            store: &'a Store,
            key: &'a ActionKey,
        ) -> futures::future::BoxFuture<'a, bool> {
            Box::pin(async move {
                self.fetched.lock().unwrap().push(team.to_owned());
                if *key != self.held.0 {
                    return false;
                }
                let (id, size) = store.put_blob(&self.held.1).unwrap();
                let mut action = Action::new("turbo", "held", asc_buildcache::Origin::default());
                action.outputs.push(Output {
                    path: "body".into(),
                    kind: OutputKind::File,
                    size,
                    blob: hex(&id),
                });
                store.put_action(key, &action).unwrap();
                true
            })
        }

        fn send(&self, team: String, _store: Store, key: ActionKey) {
            let _ = self.sent.send((team, key));
        }
    }

    #[tokio::test]
    async fn a_turbo_miss_reads_through_the_team_server_and_an_upload_goes_on_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fake = Arc::new(Fake {
            held: (
                key("turbo", "artifact", "remote1"),
                b"from the server".to_vec(),
            ),
            fetched: std::sync::Mutex::new(Vec::new()),
            sent: tx,
        });
        let app = router(store.clone(), "tok", false, Some(fake.clone()));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, app, std::future::pending()));
        let http = reqwest::Client::new();
        let team = "asc-0123456789abcdef";
        let get = |hash: &str, q: &str| {
            http.get(format!("{base}/v8/artifacts/{hash}{q}"))
                .bearer_auth("tok")
                .send()
        };

        // A local miss is fetched from the team's server and answered as a hit.
        let res = get("remote1", &format!("?slug={team}")).await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.bytes().await.unwrap(), &b"from the server"[..]);
        // Now local: the server is not asked again.
        let res = get("remote1", &format!("?slug={team}")).await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(fake.fetched.lock().unwrap().as_slice(), [team]);

        // A miss on the server too is a miss; without a work tree's team the server is not asked.
        assert_eq!(
            get("absent", &format!("?teamId={team}"))
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(get("other", "?slug=asc").await.unwrap().status(), 404);
        assert_eq!(get("other", "").await.unwrap().status(), 404);
        assert_eq!(fake.fetched.lock().unwrap().len(), 2);

        // An upload is stored locally and handed to the server tier with its team.
        let res = http
            .put(format!("{base}/v8/artifacts/local1?slug={team}"))
            .bearer_auth("tok")
            .header(TAG, "sig")
            .body("built here")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let (t, k) = rx.recv().await.unwrap();
        assert_eq!((t.as_str(), k), (team, key("turbo", "artifact", "local1")));
        assert!(store.get_action(&k).unwrap().is_some());
        // Without a team nothing goes on.
        let res = http
            .put(format!("{base}/v8/artifacts/local2"))
            .bearer_auth("tok")
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_team_comes_from_slug_or_team_id_and_only_a_work_tree_team_counts() {
        assert_eq!(
            team(Some("slug=asc-0123456789abcdef")).as_deref(),
            Some("asc-0123456789abcdef")
        );
        assert_eq!(
            team(Some("x=1&teamId=asc-fedcba9876543210")).as_deref(),
            Some("asc-fedcba9876543210")
        );
        assert_eq!(team(Some("slug=asc")), None);
        assert_eq!(team(Some("slug=asc-..%2F")), None);
        assert_eq!(team(None), None);
    }

    #[test]
    fn the_front_file_keeps_its_port_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let first = FrontFile::ensure(&home, &dir.path().join("s1")).unwrap();
        assert_eq!(first.token.len(), 64);
        let again = FrontFile::ensure(&home, &dir.path().join("s2")).unwrap();
        assert_eq!((again.port, &again.token), (first.port, &first.token));
        assert!(again.store.ends_with("s2"));
        assert_eq!(FrontFile::load(&home), Some(again));
    }
}
