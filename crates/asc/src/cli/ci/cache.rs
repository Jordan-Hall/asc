//! `asc ci cache save|restore` (SPEC §12.7, PLAN U75), and the HTTP client of the Actions store
//! that `ci artifact` shares.
//!
//! File content goes to the repository's object store over RPC; the manifest goes through
//! `PUT|GET /actions/v1/<owner>/<repo>/<cache|artifact>/<key>` on the same server and port. The
//! token is the one the remote already resolved (environment first), never an argument.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use asc_ci::cache::{
    self, Error as CiError, ListedKey, MAX_MANIFEST_BYTES, ManifestApi, Saved, StoreKind, pct,
};
use asc_store::objects::Objects;
use clap::{Parser, Subcommand};

use crate::cli::common::{CliError, Ctx, Result};
use crate::remote::server;

/// Every flag of `ci cache`, for the native table.
pub const FLAGS: &[&str] = &["--root", "--remote", "--restore-keys"];

/// Longest wait for one request.
const TIMEOUT: Duration = Duration::from_secs(300);
/// Largest error body shown.
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Parser, Debug)]
#[command(
    name = "ci cache",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Stores the files named by `paths` under `key`.
    Save(Save),
    /// Restores the cache stored under `key`, else under the newest key with a restore-key
    /// prefix.
    Restore(Restore),
}

#[derive(clap::Args, Debug)]
struct Common {
    /// Paths are relative to this directory (default: the current one).
    #[arg(long)]
    root: Option<PathBuf>,
    /// A configured remote or a server URL (default `origin`).
    #[arg(long, env = "ASC_REMOTE")]
    remote: Option<String>,
}

#[derive(clap::Args, Debug)]
struct Save {
    key: String,
    #[command(flatten)]
    common: Common,
    /// Files, directories and globs below the root.
    paths: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct Restore {
    key: String,
    #[command(flatten)]
    common: Common,
    /// Prefixes tried in order when `key` is not found; newline separated.
    #[arg(long = "restore-keys")]
    restore_keys: Vec<String>,
    /// Accepted for `actions/cache` compatibility; the manifest names what is restored.
    #[arg(hide = true)]
    _paths: Vec<String>,
}

/// Splits `--restore-keys` values at newlines, dropping blanks.
fn split_keys(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|v| v.split('\n'))
        .map(|s| s.trim_matches(['\r', ' ']).to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

pub(super) fn fatal(e: CiError) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// The root directory of a command: `--root` or the current directory, made absolute.
pub(super) fn root_of(ctx: &Ctx, root: Option<&Path>) -> Result<PathBuf> {
    let r = match root {
        Some(r) => ctx.cwd.join(r),
        None => ctx.cwd.clone(),
    };
    Ok(std::path::absolute(r)?)
}

/// What a command needs from the server.
pub(super) struct Conn {
    pub api: HttpApi,
    pub objects: Arc<dyn Objects>,
}

/// Opens the server named by `remote` (default `origin`).
pub(super) async fn connect(ctx: &Ctx, remote: Option<&str>) -> Result<Conn> {
    let r = server::open_for(ctx, remote).await?;
    let target = r.target().clone();
    server::check_transport(&target.rpc_url)?;
    let credential = server::to_rpc(&r.session().resolved)?;
    let api = HttpApi::new(
        &target.rpc_url,
        &target.repo,
        credential.map(|c| c.header_value()),
    )?;
    Ok(Conn {
        api,
        objects: Arc::new(r.client().objects()),
    })
}

/// Appends `name=value` lines to `$GITHUB_OUTPUT` when it is set, and prints them.
pub(super) fn emit(lines: &[(&str, String)]) {
    let mut text = String::new();
    for (k, v) in lines {
        if !v.contains(['\n', '\r']) {
            text.push_str(&format!("{k}={v}\n"));
        }
    }
    let _ = std::io::stdout().write_all(text.as_bytes());
    if let Some(file) = std::env::var_os("GITHUB_OUTPUT").filter(|f| !f.is_empty())
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a = Args::try_parse_from(
        std::iter::once(OsString::from("ci cache")).chain(rest.iter().cloned()),
    )
    .map_err(|e| CliError::Usage(e.to_string()))?;
    match a.cmd {
        Cmd::Save(s) => {
            let root = root_of(ctx, s.common.root.as_deref())?;
            let c = connect(ctx, s.common.remote.as_deref()).await?;
            let out = cache::save(&c.api, &*c.objects, &root, &s.key, &s.paths)
                .await
                .map_err(fatal)?;
            match out {
                Saved::Exists => eprintln!("cache {} already exists; not saved", s.key),
                Saved::Empty => eprintln!("no file matched; nothing saved"),
                Saved::Saved { entries, stats } => eprintln!(
                    "saved {entries} files ({} bytes, {} new objects) as {}",
                    stats.bytes, stats.new_objects, s.key
                ),
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Restore(r) => {
            let root = root_of(ctx, r.common.root.as_deref())?;
            let c = connect(ctx, r.common.remote.as_deref()).await?;
            let keys = split_keys(&r.restore_keys);
            let hit = cache::restore(&c.api, &*c.objects, &root, &r.key, &keys)
                .await
                .map_err(fatal)?;
            match hit {
                Some(h) => {
                    eprintln!(
                        "restored {} files ({} bytes) from {}",
                        h.stats.files, h.stats.bytes, h.matched
                    );
                    emit(&[
                        ("cache-hit", h.exact.to_string()),
                        ("cache-matched-key", h.matched),
                    ]);
                }
                None => {
                    eprintln!("no cache found for {}", r.key);
                    emit(&[("cache-hit", "false".to_owned())]);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

// ---------------------------------------------------------------------------------- http

/// The manifest endpoints over HTTP(S).
pub struct HttpApi {
    client: reqwest::Client,
    /// Remembers which https origins serve h3 and sends their requests there (U89b).
    h3: &'static asc_rpc::h3::Upgrader,
    /// `https://host/actions/v1/<owner>/<repo>`.
    base: String,
    auth: Option<String>,
}

impl HttpApi {
    /// `server` is the origin of the asc server (the RPC URL), `repo` is `owner/name`.
    pub fn new(server: &str, repo: &str, auth: Option<String>) -> Result<Self> {
        let (owner, name) = repo
            .split_once('/')
            .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'))
            .ok_or_else(|| CliError::Fatal(format!("asc: invalid repository name '{repo}'")))?;
        let client = reqwest::Client::builder()
            .redirect(crate::credential::redirect_policy())
            .timeout(TIMEOUT)
            .user_agent(concat!("asc/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| CliError::Fatal(format!("asc: http client: {e}")))?;
        Ok(Self {
            client,
            h3: asc_rpc::h3::Upgrader::shared(),
            base: format!(
                "{}/actions/v1/{}/{}",
                server.trim_end_matches('/'),
                pct(owner),
                pct(name)
            ),
            auth,
        })
    }

    /// The request as plain HTTP, for h3.
    fn h3_request(
        &self,
        method: &reqwest::Method,
        url: &str,
        body: &Bytes,
    ) -> Option<http::Request<Bytes>> {
        let mut req = http::Request::builder().method(method).uri(url).header(
            http::header::USER_AGENT,
            concat!("asc/", env!("CARGO_PKG_VERSION")),
        );
        if let Some(a) = &self.auth {
            let mut v = http::HeaderValue::from_str(a).ok()?;
            v.set_sensitive(true);
            req = req.header(http::header::AUTHORIZATION, v);
        }
        req.body(body.clone()).ok()
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let r = self.client.request(method, url);
        match &self.auth {
            Some(a) => match reqwest::header::HeaderValue::from_str(a) {
                Ok(mut v) => {
                    v.set_sensitive(true);
                    r.header(reqwest::header::AUTHORIZATION, v)
                }
                Err(_) => r,
            },
            None => r,
        }
    }

    /// One request, whole answer, body read to at most `limit` bytes. Over h3 when the origin
    /// advertised it and the last h3 attempt did not fail; otherwise over h1 or h2, whose
    /// response teaches the next call (U89b).
    async fn call(
        &self,
        method: reqwest::Method,
        url: String,
        body: Bytes,
        limit: usize,
    ) -> Result<Reply, CiError> {
        if let Some(req) = self.h3_request(&method, &url, &body) {
            match self.h3.send(&req, limit).await {
                asc_rpc::h3::Attempt::Done(r) => return Ok(r),
                asc_rpc::h3::Attempt::Failed(e) => return Err(api_err(e)),
                asc_rpc::h3::Attempt::Fallback => {}
            }
        }
        let mut res = self
            .request(method, url)
            .body(body)
            .send()
            .await
            .map_err(api_err)?;
        if let Ok(served_by) = res.url().as_str().parse::<http::Uri>() {
            self.h3.note_response(&served_by, res.headers());
        }
        let (status, headers) = (res.status(), res.headers().clone());
        let mut out = Vec::new();
        let mut truncated = res.content_length().is_some_and(|n| n > limit as u64);
        while !truncated && let Some(chunk) = res.chunk().await.map_err(api_err)? {
            if out.len() + chunk.len() > limit {
                truncated = true;
            } else {
                out.extend_from_slice(&chunk);
            }
        }
        Ok(Reply {
            status,
            headers,
            body: out.into(),
            truncated,
        })
    }
}

type Reply = asc_rpc::h3::Reply;

fn api_err(e: impl std::fmt::Display) -> CiError {
    CiError::Api(e.to_string())
}

/// The body of a reply that must fit in the limit it was read with.
fn whole(res: Reply) -> Result<Bytes, CiError> {
    if res.truncated {
        return Err(CiError::Api("response is too large".into()));
    }
    Ok(res.body)
}

/// A failed call as text: the status and the server's short message.
fn failure(res: Reply) -> CiError {
    let status = res.status;
    let body = if res.truncated {
        Bytes::new()
    } else {
        res.body
    };
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect();
    CiError::Api(format!("{status} {message}").trim_end().to_owned())
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[async_trait]
impl ManifestApi for HttpApi {
    async fn put(
        &self,
        store: StoreKind,
        key: &str,
        body: Bytes,
        overwrite: bool,
    ) -> Result<bool, CiError> {
        let mut url = format!("{}/{}/{}", self.base, store.as_str(), pct(key));
        if overwrite {
            url.push_str("?overwrite=1");
        }
        let res = self
            .call(reqwest::Method::PUT, url, body, MAX_ERROR_BYTES)
            .await?;
        match res.status {
            s if s == reqwest::StatusCode::CREATED || s.is_success() => Ok(true),
            reqwest::StatusCode::CONFLICT => Ok(false),
            _ => Err(failure(res)),
        }
    }

    async fn get(&self, store: StoreKind, key: &str) -> Result<Option<Bytes>, CiError> {
        let url = format!("{}/{}/{}", self.base, store.as_str(), pct(key));
        let res = self
            .call(reqwest::Method::GET, url, Bytes::new(), MAX_MANIFEST_BYTES)
            .await?;
        match res.status {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => Ok(Some(whole(res)?)),
            _ => Err(failure(res)),
        }
    }

    async fn find_prefix(
        &self,
        store: StoreKind,
        prefix: &str,
    ) -> Result<Option<(String, Bytes)>, CiError> {
        let url = format!("{}/{}?prefix={}", self.base, store.as_str(), pct(prefix));
        let res = self
            .call(reqwest::Method::GET, url, Bytes::new(), MAX_MANIFEST_BYTES)
            .await?;
        match res.status {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => {
                let key = res
                    .headers
                    .get("x-asc-key")
                    .and_then(|v| v.to_str().ok())
                    .and_then(percent_decode)
                    .ok_or_else(|| CiError::Api("response names no key".into()))?;
                Ok(Some((key, whole(res)?)))
            }
            _ => Err(failure(res)),
        }
    }

    async fn list(&self, store: StoreKind, prefix: &str) -> Result<Vec<ListedKey>, CiError> {
        let url = format!(
            "{}/{}?list=1&prefix={}",
            self.base,
            store.as_str(),
            pct(prefix)
        );
        let res = self
            .call(reqwest::Method::GET, url, Bytes::new(), 4 * 1024 * 1024)
            .await?;
        if !res.status.is_success() {
            return Err(failure(res));
        }
        let body = whole(res)?;
        serde_json::from_slice(&body).map_err(api_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_keys_split_at_newlines() {
        let v = vec!["a-\r\nb- \n\n".to_owned(), "c".to_owned()];
        assert_eq!(split_keys(&v), ["a-", "b-", "c"]);
    }

    #[test]
    fn urls_encode_owner_repo_and_key() {
        let a = HttpApi::new("https://h:8080/", "ow/re.po", None).unwrap();
        assert_eq!(a.base, "https://h:8080/actions/v1/ow/re.po");
        assert!(HttpApi::new("https://h", "noslash", None).is_err());
        assert!(HttpApi::new("https://h", "a/b/c", None).is_err());
    }

    #[test]
    fn key_headers_decode() {
        assert_eq!(percent_decode("a%2Fb%20c").as_deref(), Some("a/b c"));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%4"), None);
    }
}

/// The Actions-store client over HTTP/3 (U89b), against a real `asc serve` h3 listener.
#[cfg(all(test, feature = "http3", feature = "serve"))]
mod h3_tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use asc_rpc::h3::Upgrader;
    use asc_serve::h3_listener::{self, H3LimitsConfig};
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};

    use super::*;

    #[derive(Default)]
    struct Store {
        entries: Mutex<HashMap<String, Bytes>>,
        requests: Mutex<usize>,
    }

    async fn put_key(
        State(s): State<Arc<Store>>,
        Path((_, _, _, key)): Path<(String, String, String, String)>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        *s.requests.lock().unwrap() += 1;
        if headers
            .get("authorization")
            .is_none_or(|v| v != "Bearer t0k")
        {
            return StatusCode::UNAUTHORIZED;
        }
        s.entries.lock().unwrap().insert(key, body);
        StatusCode::CREATED
    }

    async fn get_key(
        State(s): State<Arc<Store>>,
        Path((_, _, _, key)): Path<(String, String, String, String)>,
        headers: HeaderMap,
    ) -> (StatusCode, Bytes) {
        *s.requests.lock().unwrap() += 1;
        if headers
            .get("authorization")
            .is_none_or(|v| v != "Bearer t0k")
        {
            return (
                StatusCode::UNAUTHORIZED,
                Bytes::from_static(br#"{"error":{"message":"bad token"}}"#),
            );
        }
        match s.entries.lock().unwrap().get(&key) {
            Some(b) => (StatusCode::OK, b.clone()),
            None => (StatusCode::NOT_FOUND, Bytes::new()),
        }
    }

    fn server() -> (h3_listener::Handle, u16, Vec<u8>, Arc<Store>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "asc h3 test ca");
        let ca = CertifiedIssuer::self_signed(ca, ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        leaf.distinguished_name
            .push(DnType::CommonName, "asc h3 test leaf");
        let leaf = leaf.signed_by(&leaf_key, &ca).unwrap();
        let store = Arc::new(Store::default());
        let router = Router::new()
            .route(
                "/actions/v1/{owner}/{repo}/{kind}/{key}",
                axum::routing::get(get_key).put(put_key),
            )
            .with_state(store.clone());
        let limits = H3LimitsConfig::default();
        let ep = h3_listener::bind(
            leaf.pem().as_bytes(),
            leaf_key.serialize_pem().as_bytes(),
            SocketAddr::from(([127, 0, 0, 1], 0)),
            &limits,
        )
        .unwrap();
        let handle = h3_listener::spawn(ep, router, limits);
        let port = handle.local_addr().unwrap().port();
        (handle, port, ca.der().to_vec(), store)
    }

    fn api(ca: Vec<u8>, port: u16, token: &str) -> HttpApi {
        let up = Box::leak(Box::new(Upgrader::with_extra_roots(
            vec![rustls::pki_types::CertificateDer::from(ca)],
            Default::default(),
        )));
        let mut advert = http::HeaderMap::new();
        advert.insert(
            http::header::ALT_SVC,
            format!("h3=\":{port}\"; ma=60").parse().unwrap(),
        );
        up.note_response(&"https://127.0.0.1:8443/".parse().unwrap(), &advert);
        let mut api = HttpApi::new(
            "https://127.0.0.1:8443",
            "o/r",
            Some(format!("Bearer {token}")),
        )
        .unwrap();
        api.h3 = up;
        api
    }

    #[tokio::test]
    async fn the_store_client_moves_manifests_over_h3_after_alt_svc() {
        let (_h, port, ca, store) = server();
        let api = api(ca, port, "t0k");
        assert!(
            api.put(
                StoreKind::Cache,
                "k1",
                Bytes::from_static(b"manifest"),
                false
            )
            .await
            .unwrap()
        );
        assert_eq!(
            api.get(StoreKind::Cache, "k1").await.unwrap().as_deref(),
            Some(&b"manifest"[..])
        );
        assert_eq!(api.get(StoreKind::Cache, "nope").await.unwrap(), None);
        assert_eq!(*store.requests.lock().unwrap(), 3, "all three went over h3");
    }

    #[tokio::test]
    async fn the_bearer_token_crosses_h3_and_a_refusal_reads_like_h2s() {
        let (_h, port, ca, store) = server();
        let api = api(ca, port, "wrong");
        let err = api
            .get(StoreKind::Cache, "k1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401") && err.contains("bad token"), "{err}");
        assert!(
            api.put(StoreKind::Cache, "k1", Bytes::from_static(b"x"), false)
                .await
                .is_err()
        );
        assert!(store.entries.lock().unwrap().is_empty());
    }
}
