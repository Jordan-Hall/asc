//! The git smart-HTTP routes of `asc serve` (SPEC §25; PLAN U81): `GET /<owner>/<repo>[.git]/info/refs`
//! and `POST /<owner>/<repo>[.git]/git-upload-pack`, served by real `git upload-pack
//! --stateless-rpc` over the repo's derived git view. Only built with the Cargo feature
//! `git-compat`; without it, or with `[git] enabled = false`, `authn::git_router` answers the
//! disabled stub on the same paths.
//!
//! The order of a request (SPEC §25.2 and §25.8):
//!
//! 1. admission (class `git`, in `limits`, before authentication) and the U78 credential edge
//!    (`authn::guard`: 400 for a malformed header or a token in the query string);
//! 2. the repository path, strictly (`RepoPath`): URL text never reaches the file system, and a
//!    hostile name is a `404` that starts nothing;
//! 3. one `Authorizer` read check: `401` with the challenge for no or a bad credential, the same
//!    `404` as a missing repo for a principal that cannot read;
//! 4. the service (`info/refs?service=` is `git-upload-pack` or `git-receive-pack`, nothing
//!    else), the registry lookup to the repo id, and the receive-pack gate (a mirror is `403`
//!    read-only; a principal without write access is `403`);
//! 5. for a fetch POST: content type and encoding (`415`), the bounded and inflated body (`413`),
//!    the `0000` probe answered here;
//! 6. `GitViewService::ensure` (the view in step with the shared refs, `503` + `Retry-After`
//!    while a large one builds), then `upload-pack` streamed to the client.
//!
//! Refs advertised are the view's: shared branches and tags only, with the ids of `git_map`
//! (the export's ids). `refs/pull`, `refs/users` and `refs/snapshots` are hidden by
//! `transfer.hideRefs` as well as absent from the view.
//!
//! Every request writes one audit record (`target = "asc_git_audit"`) and counts into the
//! `asc_git_*` metrics. Neither holds a credential.

use std::collections::VecDeque;
use std::error::Error as StdError;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use asc_git::compat::upload::{
    advertisement_prefix, advertisement_type, body_encoding, content_type_matches,
    prepare_upload_body, request_type, result_type,
};
use asc_git::compat::{
    BodyError, Endpoint, GitLimits, GitProtocol, KillReason, PackRequest, PackService,
    ProtocolVersion, RepoPath, ServerChild, Service, UploadBody, ViewError,
};
use asc_rpc::auth::{Access, AuthError, Authorizer, Credential, Principal};
use asc_rpc::server::RepoName;
use futures::StreamExt;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::authn::{self, Surface};
use crate::config::ServeConfig;
use crate::git_view::{GitViewError, GitViewService, ViewAnswer};
use crate::http::{Labels, Metrics};
use crate::registry::{Extensions, GitRouterFactory, Registry, RepoEntry, RepoMode};

/// What `Version.features` lists while the layer is live (SPEC §25.9).
pub const FEATURE: &str = "git.smart_http";

/// Bytes read from a child's stdout at once.
const CHUNK: usize = 64 * 1024;
/// Chunks waiting between the child's reader thread and the HTTP body.
const QUEUE: usize = 4;
/// How often a reader thread blocked on a slow client checks its deadlines.
const SEND_POLL: Duration = Duration::from_millis(10);
/// `Retry-After` while a view is missing or its mirror has not been fetched.
const NOT_READY_RETRY: Duration = Duration::from_secs(5);
/// The text of the receive-pack answer until U82 serves pushes.
const PUSH_NOT_SERVED: &str = "asc: pushing over the git protocol is not available yet\n";
/// The text for a principal that may read but not write.
const NO_WRITE: &str = "asc: this credential does not allow pushing to the repository\n";
/// The text for a mirror-mode repository (SPEC §25.6).
const READ_ONLY_MIRROR: &str =
    "asc: this repository is a read-only mirror; push to its upstream repository\n";
/// The text for `service=` values and paths that are not served.
const UNSUPPORTED_SERVICE: &str = "asc: this git service is not supported\n";

// ------------------------------------------------------------------------------ metrics

/// The `asc_git_*` metrics of SPEC §25.8 item 9 that belong to the fetch path.
pub struct GitMetrics {
    requests: Family<Labels, Counter>,
    bytes: Family<Labels, Counter>,
    child_seconds: Histogram,
    rejected: Family<Labels, Counter>,
}

impl GitMetrics {
    /// Registers the metrics on `metrics` (exposed on the admin listener).
    pub fn register(metrics: &Metrics) -> Arc<Self> {
        let m = Arc::new(Self {
            requests: Family::default(),
            bytes: Family::default(),
            child_seconds: Histogram::new(exponential_buckets(0.001, 2.0, 16)),
            rejected: Family::default(),
        });
        metrics.with_registry(|r| {
            r.register(
                "asc_git_requests",
                "Git protocol requests by service, protocol version and status",
                m.requests.clone(),
            );
            r.register(
                "asc_git_bytes",
                "Git protocol bytes by direction",
                m.bytes.clone(),
            );
            r.register(
                "asc_git_child_seconds",
                "Wall time of git children",
                m.child_seconds.clone(),
            );
            r.register(
                "asc_git_rejected",
                "Git protocol requests refused, by reason",
                m.rejected.clone(),
            );
        });
        m
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn request(&self, service: &str, version: &str, status: StatusCode) {
        self.requests
            .get_or_create(&Self::labels(&[
                ("service", service),
                ("version", version),
                ("status", status.as_str()),
            ]))
            .inc();
    }

    fn bytes(&self, dir: &str, n: u64) {
        self.bytes
            .get_or_create(&Self::labels(&[("dir", dir)]))
            .inc_by(n);
    }

    fn reject(&self, reason: &str) {
        self.rejected
            .get_or_create(&Self::labels(&[("reason", reason)]))
            .inc();
    }
}

// ------------------------------------------------------------------------------ audit

/// One audit record per request (SPEC §25.8 item 9). Never a credential or a `Git-Protocol`
/// value other than the version.
#[derive(Clone)]
struct Audit {
    method: &'static str,
    principal: Option<String>,
    repo: String,
    service: &'static str,
    version: &'static str,
    peer: Option<IpAddr>,
    bytes_in: u64,
    op_id: Uuid,
    started: Instant,
}

impl Audit {
    fn emit(&self, outcome: &str, status: StatusCode, bytes_out: u64) {
        tracing::info!(
            target: "asc_git_audit",
            method = self.method,
            principal = self.principal.as_deref().unwrap_or("-"),
            repo = %self.repo,
            service = self.service,
            version = self.version,
            peer = self.peer.map(|p| p.to_string()).unwrap_or_default(),
            bytes_in = self.bytes_in,
            bytes_out,
            status = status.as_u16(),
            op_id = %self.op_id,
            outcome,
            millis = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "git request"
        );
    }
}

fn version_label(p: Option<&GitProtocol>) -> &'static str {
    use asc_git::compat::ProtocolVersion::{V0, V1, V2};
    match p.map(GitProtocol::version) {
        None | Some(V0) => "v0",
        Some(V1) => "v1",
        Some(V2) => "v2",
    }
}

// ------------------------------------------------------------------------------ state

/// Everything a git route needs.
#[derive(Clone)]
pub struct GitHttp {
    views: Arc<GitViewService>,
    registry: Arc<Registry>,
    authorizer: Arc<dyn Authorizer>,
    metrics: Arc<GitMetrics>,
    limits: Arc<GitLimits>,
}

/// A refused request: the status, the reason for metrics and audit, and the answer.
struct Refusal {
    status: StatusCode,
    reason: &'static str,
    response: Response,
    /// The reader thread already wrote the audit record and counted the request.
    recorded: bool,
}

impl Refusal {
    fn plain(status: StatusCode, reason: &'static str, body: &'static str) -> Self {
        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        Self {
            status,
            reason,
            response,
            recorded: false,
        }
    }

    fn empty(status: StatusCode, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            response: status.into_response(),
            recorded: false,
        }
    }

    fn not_found(reason: &'static str) -> Self {
        Self::empty(StatusCode::NOT_FOUND, reason)
    }

    fn retry(status: StatusCode, reason: &'static str, after: Duration, body: &'static str) -> Self {
        let mut r = Self::plain(status, reason, body);
        let secs = after.as_secs().max(1);
        r.response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        r
    }

    fn internal(reason: &'static str) -> Self {
        Self::plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            reason,
            "asc: the git service failed\n",
        )
    }
}

/// How a request ended without streaming.
enum Reply {
    /// A complete answer (the probe).
    Done(Response),
    /// A child is streaming; its reader thread writes the audit record and the metrics.
    Streaming(Response),
}

fn no_cache(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, max-age=0, must-revalidate"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::EXPIRES,
        HeaderValue::from_static("Fri, 01 Jan 1980 00:00:00 GMT"),
    );
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The `Git-Protocol` header when it is a valid protocol request; anything else is dropped.
fn protocol_of(headers: &HeaderMap) -> Option<GitProtocol> {
    let mut values = headers.get_all("git-protocol").iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    GitProtocol::parse(first.to_str().ok()?)
}

fn rate_limited(e: &(dyn StdError + 'static)) -> bool {
    let mut cur = Some(e);
    while let Some(err) = cur {
        if let Some(status) = err.downcast_ref::<tonic::Status>() {
            return status.code() == tonic::Code::ResourceExhausted;
        }
        cur = err.source();
    }
    false
}

/// Why the body of a POST could not be read.
enum BodyFail {
    TooLarge,
    Timeout,
    Limited,
    Broken,
}

/// Reads the whole body, at most `cap` bytes, within `deadline` and with no more than `idle`
/// between chunks.
async fn read_body(
    body: Body,
    cap: u64,
    idle: Duration,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, BodyFail> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    loop {
        let wait = (tokio::time::Instant::now() + idle).min(deadline);
        match tokio::time::timeout_at(wait, stream.next()).await {
            Err(_) => return Err(BodyFail::Timeout),
            Ok(None) => return Ok(out),
            Ok(Some(Err(e))) => {
                return Err(if rate_limited(&e) {
                    BodyFail::Limited
                } else {
                    BodyFail::Broken
                });
            }
            Ok(Some(Ok(chunk))) => {
                if (out.len() as u64).saturating_add(chunk.len() as u64) > cap {
                    return Err(BodyFail::TooLarge);
                }
                out.extend_from_slice(&chunk);
            }
        }
    }
}

// ------------------------------------------------------------------------------ the child

/// What a finished child left for the audit record.
struct ChildReport {
    ok: bool,
    killed: Option<KillReason>,
    elapsed: Duration,
}

enum Msg {
    Data(Bytes),
    End(ChildReport),
}

/// What the thread that owns a child does when it is done.
type Finish = Box<dyn FnOnce(&ChildReport, bool, u64) + Send>;

/// Sends `msg`, waiting while the client reads slowly, up to the child's idle and wall limits.
fn send_bounded(
    tx: &mpsc::Sender<Msg>,
    mut msg: Msg,
    idle: Duration,
    deadline: Instant,
) -> bool {
    let stalled = Instant::now();
    loop {
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(back)) => {
                if stalled.elapsed() >= idle || Instant::now() >= deadline {
                    return false;
                }
                msg = back;
                std::thread::sleep(SEND_POLL);
            }
        }
    }
}

/// Runs `child` on its own threads: one feeds `input` to its stdin, this one reads stdout to the
/// HTTP body through `tx`, then waits for it and calls `finish`.
fn drive(
    mut child: ServerChild,
    input: Option<Vec<u8>>,
    tx: mpsc::Sender<Msg>,
    limits: Arc<GitLimits>,
    finish: Finish,
) {
    let std_thread = std::thread::Builder::new().name("asc-git-http".into());
    let spawned = std_thread.spawn(move || {
        let activity = child.activity();
        let deadline = Instant::now() + limits.request_timeout;
        let stdin = child.stdin.take();
        let writer = match (stdin, input) {
            (Some(mut stdin), Some(body)) => {
                let act = activity.clone();
                std::thread::Builder::new()
                    .name("asc-git-http-in".into())
                    .spawn(move || {
                        for part in body.chunks(CHUNK) {
                            if stdin.write_all(part).is_err() {
                                return;
                            }
                            act.touch();
                        }
                    })
                    .ok()
            }
            // An advertisement reads nothing: closing stdin lets the child see its end.
            _ => None,
        };
        let mut out_bytes = 0u64;
        let mut sent_any = false;
        if let Some(mut stdout) = child.stdout.take() {
            let mut buf = vec![0u8; CHUNK];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        activity.touch();
                        out_bytes += n as u64;
                        let msg = Msg::Data(Bytes::copy_from_slice(&buf[..n]));
                        if !send_bounded(&tx, msg, limits.idle_timeout, deadline) {
                            // The client went away or stopped reading: the child has no reader.
                            child.kill();
                            break;
                        }
                        sent_any = true;
                    }
                }
            }
        }
        if let Some(w) = writer {
            let _ = w.join();
        }
        let report = match child.wait() {
            Ok(exit) => ChildReport {
                ok: exit.status.success(),
                killed: exit.killed,
                elapsed: exit.elapsed,
            },
            Err(_) => ChildReport {
                ok: false,
                killed: None,
                elapsed: Duration::ZERO,
            },
        };
        finish(&report, sent_any, out_bytes);
        let _ = send_bounded(&tx, Msg::End(report), limits.idle_timeout, deadline);
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the git reader thread");
    }
}

/// The body of a streaming answer: `head` first, then what the reader thread sends. A child that
/// failed after the answer began ends the body with an error, so the client sees a broken
/// transfer and never a clean end.
fn stream_body(head: Vec<Bytes>, rx: mpsc::Receiver<Msg>) -> Body {
    let state = (VecDeque::from(head), rx, false);
    let stream = futures::stream::unfold(state, |(mut head, mut rx, done)| async move {
        if let Some(b) = head.pop_front() {
            return Some((Ok::<Bytes, std::io::Error>(b), (head, rx, done)));
        }
        if done {
            return None;
        }
        match rx.recv().await {
            Some(Msg::Data(b)) => Some((Ok(b), (head, rx, false))),
            Some(Msg::End(report)) if report.ok => None,
            Some(Msg::End(_)) | None => Some((
                Err(std::io::Error::other("the git child failed")),
                (head, rx, true),
            )),
        }
    });
    Body::from_stream(stream)
}

// ------------------------------------------------------------------------------ the handler

impl GitHttp {
    fn authorize_error(e: AuthError) -> Refusal {
        match e {
            AuthError::Unauthenticated => Refusal::empty(StatusCode::UNAUTHORIZED, "unauthenticated"),
            AuthError::Malformed => Refusal::empty(StatusCode::BAD_REQUEST, "malformed"),
            AuthError::Forbidden => Refusal::not_found("no_read"),
        }
    }

    async fn authorize(
        &self,
        credential: Option<&Credential>,
        name: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.authorizer
            .authorize_credential(credential, name, access)
            .await
    }

    /// The registry's repo, or the `404`/`500` to answer.
    async fn repo(&self, name: &str) -> Result<Arc<RepoEntry>, Refusal> {
        let Ok(name) = RepoName::parse(name) else {
            return Err(Refusal::not_found("bad_name"));
        };
        match self.registry.open(&name).await {
            Ok(r) => Ok(r),
            Err(crate::registry::RegistryError::NotFound(_)) => Err(Refusal::not_found("no_repo")),
            Err(e) => {
                tracing::warn!(error = %e, "opening a repository for git");
                Err(Refusal::internal("registry"))
            }
        }
    }

    /// Brings the view in step, or answers `503`/`500`.
    async fn ready_view(
        &self,
        repo: &Arc<RepoEntry>,
    ) -> Result<asc_git::compat::GitView, Refusal> {
        match self.views.ensure(repo, self.registry.refs()).await {
            Ok(ViewAnswer::Ready(_)) => {}
            Ok(ViewAnswer::Building { retry_after }) => {
                return Err(Refusal::retry(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "view_building",
                    retry_after,
                    "asc: the repository is being prepared for git; retry shortly\n",
                ));
            }
            Err(GitViewError::View(ViewError::NotARepository(_))) => {
                return Err(Refusal::retry(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "mirror_missing",
                    NOT_READY_RETRY,
                    "asc: the mirror has not been fetched yet; retry shortly\n",
                ));
            }
            Err(GitViewError::TooManyViews) => {
                return Err(Refusal::retry(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too_many_views",
                    NOT_READY_RETRY,
                    "asc: the server is busy; retry shortly\n",
                ));
            }
            Err(e) => {
                tracing::warn!(repo = %repo.name, error = %e, "syncing the git view");
                return Err(Refusal::internal("view_sync"));
            }
        }
        self.views.view(repo).map_err(|e| {
            tracing::warn!(repo = %repo.name, error = %e, "opening the git view");
            Refusal::internal("view_open")
        })
    }

    /// Starts `upload-pack` on the view and returns the streaming answer (or why it failed
    /// before its first byte).
    #[allow(clippy::too_many_arguments)]
    async fn stream_upload(
        &self,
        view: asc_git::compat::GitView,
        advertise: bool,
        protocol: Option<GitProtocol>,
        prefix: Vec<u8>,
        input: Option<Vec<u8>>,
        content_type: String,
        audit: Audit,
    ) -> Result<Response, Refusal> {
        let req = PackRequest {
            advertise_refs: advertise,
            protocol,
        };
        let spawned = tokio::task::spawn_blocking(move || {
            view.spawn_pack(PackService::UploadPack, &req)
        })
        .await;
        let child = match spawned {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::warn!(repo = %audit.repo, error = %e, "starting upload-pack");
                return Err(Refusal::internal("spawn"));
            }
            Err(_) => return Err(Refusal::internal("spawn")),
        };
        let (tx, mut rx) = mpsc::channel::<Msg>(QUEUE);
        let metrics = self.metrics.clone();
        let (service, version) = (audit.service, audit.version);
        let finish: Finish = Box::new(move |report, sent_any, out_bytes| {
            let status = if sent_any {
                StatusCode::OK
            } else if report.killed.is_some() {
                StatusCode::GATEWAY_TIMEOUT
            } else if report.ok {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            let outcome = match (report.killed, report.ok) {
                (Some(KillReason::Deadline | KillReason::Idle), _) => "killed",
                (Some(KillReason::Requested), _) => "limit:client_gone",
                (None, true) => "ok",
                (None, false) => "rejected:child_failed",
            };
            metrics.request(service, version, status);
            metrics.bytes("out", out_bytes);
            metrics.bytes("in", audit.bytes_in);
            metrics
                .child_seconds
                .observe(report.elapsed.as_secs_f64());
            if report.killed.is_some() {
                metrics.reject("killed");
            }
            audit.emit(outcome, status, out_bytes);
        });
        drive(child, input, tx, self.limits.clone(), finish);

        let first = rx.recv().await;
        let head = match first {
            Some(Msg::Data(b)) => {
                let mut head = Vec::with_capacity(2);
                if !prefix.is_empty() {
                    head.push(Bytes::from(prefix));
                }
                head.push(b);
                head
            }
            Some(Msg::End(report)) if report.ok => {
                if prefix.is_empty() {
                    Vec::new()
                } else {
                    vec![Bytes::from(prefix)]
                }
            }
            Some(Msg::End(report)) => {
                let mut refusal = match report.killed {
                    Some(KillReason::Deadline | KillReason::Idle) => Refusal::plain(
                        StatusCode::GATEWAY_TIMEOUT,
                        "timeout",
                        "asc: the git request took too long\n",
                    ),
                    _ => Refusal::internal("child_failed"),
                };
                refusal.recorded = true;
                return Err(refusal);
            }
            None => return Err(Refusal::internal("child_lost")),
        };
        let mut response = Response::new(stream_body(head, rx));
        let h = response.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&content_type) {
            h.insert(header::CONTENT_TYPE, v);
        }
        no_cache(h);
        Ok(response)
    }

    async fn run(
        &self,
        endpoint: Endpoint,
        owner: &str,
        repo: &str,
        req: Request,
        audit: &mut Audit,
    ) -> Result<Reply, Refusal> {
        let path = RepoPath::parse(owner, repo).map_err(|_| Refusal::not_found("bad_path"))?;
        let name = path.full_name();
        audit.repo.clone_from(&name);

        let (parts, body) = req.into_parts();
        let (want_method, allow) = match endpoint {
            Endpoint::InfoRefs => (Method::GET, "GET"),
            Endpoint::UploadPack | Endpoint::ReceivePack => (Method::POST, "POST"),
        };
        if parts.method != want_method {
            let mut r = Refusal::empty(StatusCode::METHOD_NOT_ALLOWED, "method");
            r.response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(allow));
            return Err(r);
        }
        audit.method = allow_label(&parts.method);

        // Authentication and the one read check (SPEC §25.4).
        let credential = match authn::credential(&parts.headers) {
            Ok(c) => c,
            Err(AuthError::Malformed) => {
                return Err(Refusal::empty(StatusCode::BAD_REQUEST, "malformed"));
            }
            Err(_) => return Err(Refusal::empty(StatusCode::UNAUTHORIZED, "unauthenticated")),
        };
        let principal = self
            .authorize(credential.as_ref(), &name, Access::Read)
            .await
            .map_err(Self::authorize_error)?;
        audit.principal = Some(principal.id.clone());

        // The service (SPEC §25.8 item 1): only the two, from `service=` on discovery.
        let service = match endpoint {
            Endpoint::InfoRefs => {
                let wanted = parts.uri.query().and_then(|q| {
                    url::form_urlencoded::parse(q.as_bytes())
                        .find(|(k, _)| k == "service")
                        .map(|(_, v)| v.into_owned())
                });
                match wanted.as_deref().and_then(Service::parse) {
                    Some(s) => s,
                    None => {
                        return Err(Refusal::plain(
                            StatusCode::FORBIDDEN,
                            "service",
                            UNSUPPORTED_SERVICE,
                        ));
                    }
                }
            }
            Endpoint::UploadPack => Service::UploadPack,
            Endpoint::ReceivePack => Service::ReceivePack,
        };
        audit.service = service.name();

        let entry = self.repo(&name).await?;

        if service == Service::ReceivePack {
            if entry.mode == RepoMode::Mirror {
                return Err(Refusal::plain(
                    StatusCode::FORBIDDEN,
                    "read_only_mirror",
                    READ_ONLY_MIRROR,
                ));
            }
            self.authorize(credential.as_ref(), &name, Access::Write)
                .await
                .map_err(|e| match e {
                    AuthError::Forbidden => {
                        Refusal::plain(StatusCode::FORBIDDEN, "no_write", NO_WRITE)
                    }
                    other => Self::authorize_error(other),
                })?;
            // U82 serves pushes; until then a principal that may write is told so.
            return Err(Refusal::plain(
                StatusCode::FORBIDDEN,
                "push_unavailable",
                PUSH_NOT_SERVED,
            ));
        }

        let mut protocol = protocol_of(&parts.headers);
        // A mirror holds its upstream's hidden refs (pull requests, …) with their objects. In
        // protocol v2 a client may `want` any object that exists, whatever was advertised; v0 and
        // v1 make git check each `want` against the visible refs. A mirror is therefore served
        // over v0/v1: the client falls back by itself, and the answer is the same for every
        // request of the repo. A derived view holds shared refs only, so it has nothing to hide.
        if entry.mode == RepoMode::Mirror
            && protocol
                .as_ref()
                .is_some_and(|p| p.version() == ProtocolVersion::V2)
        {
            protocol = None;
        }
        audit.version = version_label(protocol.as_ref());

        match endpoint {
            Endpoint::InfoRefs => {
                let view = self.ready_view(&entry).await?;
                let prefix = advertisement_prefix(Service::UploadPack, protocol.as_ref());
                let response = self
                    .stream_upload(
                        view,
                        true,
                        protocol,
                        prefix,
                        None,
                        advertisement_type(Service::UploadPack),
                        audit.clone(),
                    )
                    .await?;
                Ok(Reply::Streaming(response))
            }
            Endpoint::UploadPack => {
                let wanted = request_type(Service::UploadPack);
                if !content_type_matches(header_str(&parts.headers, header::CONTENT_TYPE), &wanted)
                {
                    return Err(Refusal::plain(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "content_type",
                        "asc: unsupported content type\n",
                    ));
                }
                let encoding =
                    body_encoding(header_str(&parts.headers, header::CONTENT_ENCODING)).map_err(
                        |_| {
                            Refusal::plain(
                                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                                "content_encoding",
                                "asc: unsupported content encoding\n",
                            )
                        },
                    )?;
                let cap = self.limits.fetch_body_bytes;
                let deadline = tokio::time::Instant::now() + self.limits.request_timeout;
                let raw = read_body(body, cap, self.limits.idle_timeout, deadline)
                    .await
                    .map_err(|f| match f {
                        BodyFail::TooLarge => Refusal::plain(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "body_size",
                            "asc: the request is too large\n",
                        ),
                        BodyFail::Timeout => Refusal::plain(
                            StatusCode::REQUEST_TIMEOUT,
                            "body_timeout",
                            "asc: the request body stalled\n",
                        ),
                        BodyFail::Limited => Refusal::retry(
                            StatusCode::TOO_MANY_REQUESTS,
                            "rate",
                            Duration::from_secs(1),
                            "asc: too many git requests; retry later\n",
                        ),
                        BodyFail::Broken => Refusal::empty(StatusCode::BAD_REQUEST, "body"),
                    })?;
                audit.bytes_in = raw.len() as u64;
                let body = prepare_upload_body(raw, encoding, cap).map_err(|e| match e {
                    BodyError::TooLarge => Refusal::plain(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "body_size",
                        "asc: the request is too large\n",
                    ),
                    BodyError::UnsupportedEncoding => Refusal::plain(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "content_encoding",
                        "asc: unsupported content encoding\n",
                    ),
                    BodyError::Corrupt | BodyError::Empty => {
                        Refusal::empty(StatusCode::BAD_REQUEST, "body")
                    }
                })?;
                let input = match body {
                    UploadBody::Probe => {
                        // Answered before any child starts (SPEC §25.5).
                        let mut res = Response::new(Body::empty());
                        let h = res.headers_mut();
                        if let Ok(v) = HeaderValue::from_str(&result_type(Service::UploadPack)) {
                            h.insert(header::CONTENT_TYPE, v);
                        }
                        no_cache(h);
                        return Ok(Reply::Done(res));
                    }
                    UploadBody::Request(b) => b,
                };
                let view = self.ready_view(&entry).await?;
                let response = self
                    .stream_upload(
                        view,
                        false,
                        protocol,
                        Vec::new(),
                        Some(input),
                        result_type(Service::UploadPack),
                        audit.clone(),
                    )
                    .await?;
                Ok(Reply::Streaming(response))
            }
            // Returned above, with the receive-pack gate.
            Endpoint::ReceivePack => Err(Refusal::internal("receive_pack")),
        }
    }

    async fn handle(&self, endpoint: Endpoint, owner: String, repo: String, req: Request) -> Response {
        let mut audit = Audit {
            method: "-",
            principal: None,
            repo: String::new(),
            service: "unknown",
            version: "v0",
            peer: crate::limits::connect_addr(req.extensions()),
            bytes_in: 0,
            op_id: Uuid::new_v4(),
            started: Instant::now(),
        };
        let result = self.run(endpoint, &owner, &repo, req, &mut audit).await;
        match result {
            Ok(Reply::Streaming(response)) => response,
            Ok(Reply::Done(response)) => {
                self.metrics
                    .request(audit.service, audit.version, StatusCode::OK);
                audit.emit("ok", StatusCode::OK, 0);
                response
            }
            Err(refusal) => {
                if !refusal.recorded {
                    self.metrics.reject(refusal.reason);
                    self.metrics
                        .request(audit.service, audit.version, refusal.status);
                    audit.emit(
                        &format!("rejected:{}", refusal.reason),
                        refusal.status,
                        0,
                    );
                }
                let mut response = refusal.response;
                // A 401's challenge is added by the credential edge (`authn::guard`).
                no_cache(response.headers_mut());
                response
            }
        }
    }
}

fn allow_label(m: &Method) -> &'static str {
    if m == Method::GET { "GET" } else { "POST" }
}

// ------------------------------------------------------------------------------ routes

async fn info_refs(
    State(st): State<GitHttp>,
    Path((owner, repo)): Path<(String, String)>,
    req: Request,
) -> Response {
    st.handle(Endpoint::InfoRefs, owner, repo, req).await
}

async fn upload_pack(
    State(st): State<GitHttp>,
    Path((owner, repo)): Path<(String, String)>,
    req: Request,
) -> Response {
    st.handle(Endpoint::UploadPack, owner, repo, req).await
}

async fn receive_pack(
    State(st): State<GitHttp>,
    Path((owner, repo)): Path<(String, String)>,
    req: Request,
) -> Response {
    st.handle(Endpoint::ReceivePack, owner, repo, req).await
}

/// The live git routes behind the U78 credential edge.
pub fn router(state: GitHttp) -> Router {
    let routes = Router::new()
        .route("/{owner}/{repo}/info/refs", any(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", any(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", any(receive_pack))
        .with_state(state);
    authn::guard(routes, Surface::Api)
}

/// One registration line in `lib.rs`: the git routes, served in place of the disabled stub.
pub fn register(
    ext: &Extensions,
    views: Arc<GitViewService>,
    registry: Arc<Registry>,
    metrics: &Arc<Metrics>,
    _cfg: &ServeConfig,
) {
    let limits = Arc::new(views.server().limits().clone());
    let metrics = GitMetrics::register(metrics);
    let factory: GitRouterFactory = Arc::new(move |authorizer| {
        router(GitHttp {
            views: views.clone(),
            registry: registry.clone(),
            authorizer,
            metrics: metrics.clone(),
            limits: limits.clone(),
        })
    });
    if ext.git.set(factory).is_err() {
        tracing::warn!("the git routes were already registered; keeping the first");
    }
}
