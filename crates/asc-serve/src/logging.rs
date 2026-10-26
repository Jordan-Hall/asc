//! Logging for `asc serve`: the subscriber, the startup lines, the access log and the principal
//! that the access log names.
//!
//! The library crates only emit `tracing` events. This module is the one place that installs a
//! subscriber, and only `asc serve` calls [`init`]. Nothing here logs a credential: the access log
//! records the method, the path without its query, the status, the latency, the principal id and
//! a request id, and the startup lines show URLs without user info, query or fragment.
//!
//! - `RUST_LOG` sets the level filter (default `info`; `RUST_LOG=asc_serve::access=off` silences
//!   the access log).
//! - `ASC_LOG_FORMAT=json` or `[log] format = "json"` selects JSON lines (the variable wins).
//! - Output goes to stderr, which is unbuffered, so `docker logs` shows it at once.

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Credential, Principal};
use futures::future::BoxFuture;
use tower::{Layer, Service};
use tracing::{Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config::{LogConfig, LogFormat, ServeConfig};

/// The environment variable that overrides `[log] format`.
pub const FORMAT_ENV: &str = "ASC_LOG_FORMAT";
/// The `tracing` target of the access log lines.
pub const ACCESS_TARGET: &str = "asc_serve::access";
/// The header that carries a request id in and out.
const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
/// Longest accepted caller-supplied request id.
const MAX_REQUEST_ID: usize = 64;
/// Longest path or principal id written to a log line.
const MAX_FIELD: usize = 256;
/// One warning per this many refused credentials.
const AUTH_WARN_EVERY: u64 = 100;
/// Paths that are logged at debug: probes and scrapes, not traffic.
const QUIET_PATHS: [&str; 3] = ["/healthz", "/readyz", "/metrics"];

// ------------------------------------------------------------------------------ subscriber

/// A subscriber that writes `format` lines to `writer`, filtered by `filter`.
pub fn build<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
    ansi: bool,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(ansi);
    match format {
        LogFormat::Json => Box::new(builder.json().flatten_event(true).finish()),
        LogFormat::Text => Box::new(builder.finish()),
    }
}

/// The format `ASC_LOG_FORMAT` names, if it names one. `Err` carries a value that is not a format.
fn format_from_env(value: Option<&str>) -> Result<Option<LogFormat>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("json") => Ok(Some(LogFormat::Json)),
        Some(v) if v.eq_ignore_ascii_case("text") => Ok(Some(LogFormat::Text)),
        Some(v) => Err(v.chars().take(32).collect()),
    }
}

/// The filter `RUST_LOG` gives, or `info`. `Err` carries the parse error of a bad `RUST_LOG`.
fn filter_from_env(value: Option<&str>) -> (EnvFilter, Option<String>) {
    match value.map(str::trim) {
        None | Some("") => (EnvFilter::new("info"), None),
        Some(v) => match EnvFilter::try_new(v) {
            Ok(f) => (f, None),
            Err(e) => (EnvFilter::new("info"), Some(e.to_string())),
        },
    }
}

/// Installs the process-wide subscriber for `asc serve`: stderr, `RUST_LOG`, text or JSON. A second
/// call, or a subscriber somebody else installed first, leaves things as they are. It also routes
/// the `log` records of dependencies (the SSH library) into the same output.
pub fn init(cfg: &LogConfig) {
    let env_format = std::env::var(FORMAT_ENV).ok();
    let (format, bad_format) = match format_from_env(env_format.as_deref()) {
        Ok(f) => (f.unwrap_or(cfg.format), None),
        Err(v) => (cfg.format, Some(v)),
    };
    let (filter, bad_filter) = filter_from_env(std::env::var("RUST_LOG").ok().as_deref());
    let ansi = std::io::stderr().is_terminal();
    if build(format, filter, std::io::stderr, ansi)
        .try_init()
        .is_err()
    {
        return;
    }
    if let Some(v) = bad_format {
        tracing::warn!(value = %v, "{FORMAT_ENV} is neither `text` nor `json`; using the config");
    }
    if let Some(e) = bad_filter {
        tracing::warn!(error = %e, "RUST_LOG is not a valid filter; using `info`");
    }
}

// ------------------------------------------------------------------------------ redaction

/// `raw` as `scheme://host[:port]/path`: no user info, query or fragment, which is where URLs
/// keep passwords and keys.
pub fn redact_url(raw: &str) -> String {
    let Ok(u) = url::Url::parse(raw) else {
        return "<unparseable url>".to_owned();
    };
    let mut out = format!("{}://", u.scheme());
    if let Some(host) = u.host_str() {
        out.push_str(host);
        if let Some(port) = u.port() {
            out.push_str(&format!(":{port}"));
        }
    }
    out.push_str(u.path());
    out
}

/// The kind of database a DSN names. Never the DSN: it holds the password.
pub fn db_kind(dsn: &str) -> &'static str {
    let d = dsn.trim_start().to_ascii_lowercase();
    if d.starts_with("postgres") {
        "postgres"
    } else if d.starts_with("sqlite") {
        "sqlite"
    } else {
        "other"
    }
}

/// `text` cut to `max` characters, control characters replaced, so a log line stays one line.
fn clean(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

// ------------------------------------------------------------------------------ startup lines

/// The listeners that came up, for the "ready" line.
#[derive(Debug, Default)]
pub struct Listeners {
    /// The git smart-HTTP layer is serving.
    pub git: bool,
    /// The SSH listener's address and its host key fingerprints (`SHA256:...`).
    pub ssh: Option<(SocketAddr, Vec<String>)>,
    /// The QUIC listener serves HTTP/3 on this UDP port.
    pub h3: Option<u16>,
}

/// The first line: what this process was told to be. No secret reaches it.
pub fn starting(cfg: &ServeConfig) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        role = ?cfg.role,
        listen = %cfg.listen,
        admin_listen = %cfg.admin.listen,
        public_url = cfg.public_url.as_deref().map(redact_url).as_deref(),
        store = cfg.store.as_deref().map(redact_url).as_deref(),
        db = cfg.db.as_deref().map(db_kind),
        auth_provider = ?cfg.auth.provider,
        tls = cfg.tls.is_some(),
        log_format = ?cfg.log.format,
        "asc serve starting"
    );
}

/// The banner once the schema is checked and every listener is up.
pub fn ready(cfg: &ServeConfig, up: &Listeners) {
    let ssh = up.ssh.as_ref().map(|(addr, _)| addr.to_string());
    let ssh_host_keys = up
        .ssh
        .as_ref()
        .map(|(_, fingerprints)| fingerprints.join(","));
    tracing::info!(
        listen = %cfg.listen,
        migrations = "current",
        git = up.git,
        ssh = ssh.as_deref(),
        ssh_host_keys = ssh_host_keys.as_deref(),
        mcp = cfg.mcp.enabled,
        web = cfg.web.enabled,
        h3 = up.h3,
        "asc serve ready"
    );
}

// ------------------------------------------------------------------------------ access log

tokio::task_local! {
    /// What the request being served has learnt about its caller.
    static REQUEST: Arc<RequestCtx>;
}

/// Filled by [`LoggingAuthorizer`] while the request runs, read by the access log at its end.
#[derive(Default)]
struct RequestCtx {
    principal: Mutex<Option<String>>,
}

/// Tells the access log who the current request authenticated as. A call outside a request (a
/// background task, the SSH listener) does nothing.
pub fn record_principal(id: &str) {
    let _ = REQUEST.try_with(|ctx| {
        *ctx.principal.lock().unwrap_or_else(PoisonError::into_inner) = Some(clean(id, MAX_FIELD));
    });
}

/// Writes one `info` line per HTTP or RPC request (see the module docs for the fields). Probes
/// and scrapes go to `debug`, 4xx and failed gRPC calls to `warn`, 5xx to `error`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AccessLogLayer;

impl<S> Layer<S> for AccessLogLayer {
    type Service = AccessLog<S>;

    fn layer(&self, inner: S) -> AccessLog<S> {
        AccessLog { inner }
    }
}

/// The service of [`AccessLogLayer`].
#[derive(Clone, Debug)]
pub struct AccessLog<S> {
    inner: S,
}

/// A request id the caller may supply: short and made of token characters.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(&REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= MAX_REQUEST_ID
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .map_or_else(|| uuid::Uuid::new_v4().simple().to_string(), str::to_owned)
}

/// The level one finished request is logged at.
fn level_for(path: &str, status: u16, grpc_status: Option<i64>) -> Level {
    if QUIET_PATHS.contains(&path) {
        Level::DEBUG
    } else if status >= 500 {
        Level::ERROR
    } else if status >= 400 || grpc_status.is_some_and(|g| g != 0) {
        Level::WARN
    } else {
        Level::INFO
    }
}

/// One access line; the level is a runtime value, so each arm names its own macro.
macro_rules! access_line {
    ($level:expr, $($field:tt)*) => {
        match $level {
            Level::ERROR => tracing::error!(target: ACCESS_TARGET, $($field)*),
            Level::WARN => tracing::warn!(target: ACCESS_TARGET, $($field)*),
            Level::INFO => tracing::info!(target: ACCESS_TARGET, $($field)*),
            _ => tracing::debug!(target: ACCESS_TARGET, $($field)*),
        }
    };
}

impl<S, B, RB> Service<Request<B>> for AccessLog<S>
where
    S: Service<Request<B>, Response = Response<RB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: std::fmt::Display + Send + 'static,
    B: Send + 'static,
    RB: Send + 'static,
{
    type Response = Response<RB>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response<RB>, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        // The clone is the one `poll_ready` did not prepare; the ready one serves this request.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let method = req.method().clone();
        let path = clean(req.uri().path(), MAX_FIELD);
        let id = request_id(req.headers());
        let ctx = Arc::new(RequestCtx::default());
        let started = Instant::now();
        let seen = ctx.clone();
        Box::pin(REQUEST.scope(ctx, async move {
            let result = inner.call(req).await;
            let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
            let principal = seen
                .principal
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            match result {
                Ok(mut res) => {
                    let status = res.status().as_u16();
                    let grpc_status = res
                        .headers()
                        .get("grpc-status")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<i64>().ok());
                    access_line!(
                        level_for(&path, status, grpc_status),
                        method = %method,
                        path = %path,
                        status,
                        latency_ms,
                        principal = principal.as_deref(),
                        request_id = %id,
                        grpc_status,
                        "request"
                    );
                    if !res.headers().contains_key(&REQUEST_ID)
                        && let Ok(v) = HeaderValue::from_str(&id)
                    {
                        res.headers_mut().insert(REQUEST_ID, v);
                    }
                    Ok(res)
                }
                Err(e) => {
                    tracing::error!(
                        target: ACCESS_TARGET,
                        method = %method,
                        path = %path,
                        latency_ms,
                        principal = principal.as_deref(),
                        request_id = %id,
                        error = %e,
                        "request failed"
                    );
                    Err(e)
                }
            }
        }))
    }
}

// ------------------------------------------------------------------------------ authorizer

/// An [`Authorizer`] that tells the access log who each request is, and counts refusals. It
/// forwards every decision unchanged. It logs the kind of a refusal and a running count, never
/// the credential and never the reason beyond the kind.
pub struct LoggingAuthorizer {
    inner: Arc<dyn Authorizer>,
    refused: AtomicU64,
}

impl LoggingAuthorizer {
    pub fn new(inner: Arc<dyn Authorizer>) -> Self {
        Self {
            inner,
            refused: AtomicU64::new(0),
        }
    }

    /// Counts a refusal and passes the answer on. A flood of bad credentials is one warning per
    /// [`AUTH_WARN_EVERY`] refusals; the access log has the per-request lines.
    fn observe(&self, answer: Result<Principal, AuthError>) -> Result<Principal, AuthError> {
        match &answer {
            Ok(p) => record_principal(&p.id),
            Err(e) => {
                let n = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
                if n % AUTH_WARN_EVERY == 1 {
                    tracing::warn!(kind = ?e, refused_total = n, "authorization refused");
                } else {
                    tracing::debug!(kind = ?e, refused_total = n, "authorization refused");
                }
            }
        }
        answer
    }
}

#[async_trait]
impl Authorizer for LoggingAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let answer = self.inner.authorize(token, repo, access).await;
        self.observe(answer)
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let answer = self
            .inner
            .authorize_credential(credential, repo, access)
            .await;
        self.observe(answer)
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        let answer = self.inner.authenticate(credential).await;
        self.observe(answer)
    }

    async fn identify_ssh_key(
        &self,
        key: &asc_rpc::auth::SshKey,
    ) -> Result<Option<asc_rpc::auth::SshIdentity>, AuthError> {
        self.inner.identify_ssh_key(key).await
    }

    async fn authorize_ssh_key(
        &self,
        key: &asc_rpc::auth::SshKey,
        repo: &str,
        access: Access,
    ) -> Result<asc_rpc::auth::Decision, AuthError> {
        self.inner.authorize_ssh_key(key, repo, access).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_user_info_query_and_fragment() {
        assert_eq!(
            redact_url("s3://bucket/prefix?access_key=AKIA&secret=zzz#frag"),
            "s3://bucket/prefix"
        );
        assert_eq!(
            redact_url("https://user:pw@asc.example.com:8443/a/b?token=t"),
            "https://asc.example.com:8443/a/b"
        );
        assert_eq!(redact_url("memory:///"), "memory:///");
        assert_eq!(redact_url("not a url"), "<unparseable url>");
    }

    #[test]
    fn a_dsn_is_reduced_to_its_kind() {
        assert_eq!(db_kind("postgres://u:hunter2@db/asc"), "postgres");
        assert_eq!(db_kind("postgresql://u:hunter2@db/asc"), "postgres");
        assert_eq!(db_kind("sqlite:asc.db?mode=rwc"), "sqlite");
        assert_eq!(db_kind("mysql://u:hunter2@db/asc"), "other");
    }

    #[test]
    fn env_format_and_filter_fall_back_without_failing() {
        assert_eq!(format_from_env(None), Ok(None));
        assert_eq!(format_from_env(Some(" JSON ")), Ok(Some(LogFormat::Json)));
        assert_eq!(format_from_env(Some("text")), Ok(Some(LogFormat::Text)));
        assert!(format_from_env(Some("yaml")).is_err());
        assert!(filter_from_env(None).1.is_none());
        assert!(filter_from_env(Some("asc_serve=debug,warn")).1.is_none());
        assert!(filter_from_env(Some("asc_serve=loud")).1.is_some());
    }

    #[test]
    fn request_ids_are_kept_only_when_plain() {
        let mut h = HeaderMap::new();
        h.insert(REQUEST_ID, HeaderValue::from_static("abc-123_x.y"));
        assert_eq!(request_id(&h), "abc-123_x.y");
        h.insert(REQUEST_ID, HeaderValue::from_static("has space"));
        assert_eq!(request_id(&h).len(), 32);
        assert_eq!(request_id(&HeaderMap::new()).len(), 32);
    }

    #[test]
    fn levels_follow_the_outcome() {
        assert_eq!(level_for("/healthz", 200, None), Level::DEBUG);
        assert_eq!(level_for("/metrics", 503, None), Level::DEBUG);
        assert_eq!(level_for("/a/b", 200, Some(0)), Level::INFO);
        assert_eq!(level_for("/a/b", 200, Some(16)), Level::WARN);
        assert_eq!(level_for("/a/b", 404, None), Level::WARN);
        assert_eq!(level_for("/a/b", 500, None), Level::ERROR);
    }

    #[test]
    fn control_characters_cannot_split_a_line() {
        assert_eq!(clean("a\nb\r\tc", 16), "a?b??c");
        assert_eq!(clean("abcdef", 3), "abc");
    }
}
