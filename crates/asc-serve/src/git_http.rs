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
//! Pushes (U82) take the same steps up to the view, with `write` access and these differences:
//! `info/refs?service=git-receive-pack` is answered by `receive-pack --advertise-refs`; a
//! `POST git-receive-pack` is the `0000` probe (answered before any child starts), or a push that
//! holds a push slot (`429`/`503`) and passes the disk check (`503`), then streams the request
//! body (never buffered, capped at `max_push_bytes` plus the command list) into `git
//! receive-pack --stateless-rpc`. The only wait git has for asc is the `pre-receive` hook
//! ([`crate::git_push`]).
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
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use asc_git::compat::pktline;
use asc_git::compat::upload::{
    advertisement_prefix, advertisement_type, body_encoding, content_type_matches,
    prepare_upload_body, request_type, result_type,
};
use asc_git::compat::{
    BodyEncoding, BodyError, Endpoint, GitLimits, GitProtocol, KillReason, PackRequest,
    PackService, ProtocolVersion, RepoPath, ServerChild, Service, UploadBody, ViewError,
};
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, AuthError, Authorizer, Credential, Principal};
use asc_rpc::server::{CallCtx, RepoName};
use axum::Router;
use axum::body::{Body, BodyDataStream, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures::StreamExt;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::authn::{self, Surface};
use crate::config::ServeConfig;
use crate::git_push::{Busy, GitPush, HookProgram, PushPermit, PushSession};
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
/// The text when pushing is not served although the principal may write.
const PUSH_DISABLED: &str = "asc: pushing over the git protocol is not available on this server\n";
/// The text when a principal already has its pushes running (`429`).
const PUSH_BUSY_PRINCIPAL: &str =
    "asc: too many pushes are running for your account; retry later\n";
/// The text when the replica has all its push slots in use (`503`).
const PUSH_BUSY_REPLICA: &str = "asc: the server is busy with other pushes; retry later\n";
/// The text when the volume of the git views is nearly full (SPEC §25.5).
const DISK_LOW: &str = "asc: server disk is low\n";
/// `Retry-After` for a push that found no slot.
const PUSH_RETRY: Duration = Duration::from_secs(5);
/// Room the request body of a push has over `max_push_bytes`: the command list, push options and
/// pkt-line framing (SPEC §25.5 passes the cap itself to git as `receive.maxInputSize`).
const PUSH_BODY_SLACK: u64 = 8 * 1024 * 1024;
/// Why a push body was cut short (stored in an `AtomicU8`; zero is "not cut").
const CUT_TOO_LARGE: u8 = 1;
const CUT_TIMEOUT: u8 = 2;
const CUT_LIMITED: u8 = 3;
const CUT_BROKEN: u8 = 4;
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

    /// A request counted with a `transport` label: how the other transports (SSH) join the same
    /// series. The smart-HTTP series carry no `transport` label (SPEC §25.11.8).
    pub(crate) fn request_via(&self, transport: &str, service: &str, version: &str, status: u16) {
        self.requests
            .get_or_create(&Self::labels(&[
                ("transport", transport),
                ("service", service),
                ("version", version),
                ("status", &status.to_string()),
            ]))
            .inc();
    }

    pub(crate) fn bytes_via(&self, transport: &str, dir: &str, n: u64) {
        self.bytes
            .get_or_create(&Self::labels(&[("transport", transport), ("dir", dir)]))
            .inc_by(n);
    }

    pub(crate) fn reject_via(&self, transport: &str, reason: &str) {
        self.rejected
            .get_or_create(&Self::labels(&[
                ("transport", transport),
                ("reason", reason),
            ]))
            .inc();
    }

    pub(crate) fn child_seconds(&self, secs: f64) {
        self.child_seconds.observe(secs);
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
    /// Objects of a push (zero for a fetch).
    objects: u64,
    /// The refs a push updated, `name old new` for each (SPEC §25.8 item 9).
    refs: String,
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
            objects = self.objects,
            refs = %self.refs,
            status = status.as_u16(),
            op_id = %self.op_id,
            outcome,
            millis = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "git request"
        );
    }
}

pub(crate) fn version_label(p: Option<&GitProtocol>) -> &'static str {
    use asc_git::compat::ProtocolVersion::{V0, V1, V2};
    match p.map(GitProtocol::version) {
        None | Some(V0) => "v0",
        Some(V1) => "v1",
        Some(V2) => "v2",
    }
}

// ------------------------------------------------------------------------------ shared steps

/// Why a repository's git view is not ready to serve. Each transport words its own answer.
pub(crate) enum ViewWait {
    /// A large view is building in the background; retry after this long.
    Building(Duration),
    /// A mirror repository whose upstream has not been fetched yet.
    MirrorMissing,
    /// The replica holds as many views as it may.
    TooManyViews,
    /// The view could not be synced or opened; the metric reason.
    Failed(&'static str),
}

/// Brings `repo`'s view in step with the shared refs and opens it: the step every transport takes
/// before it starts a pack child (SPEC §25.2, §25.11.5).
pub(crate) async fn sync_view(
    views: &Arc<GitViewService>,
    registry: &Registry,
    repo: &Arc<RepoEntry>,
) -> Result<asc_git::compat::GitView, ViewWait> {
    match views.ensure(repo, registry.refs()).await {
        Ok(ViewAnswer::Ready(_)) => {}
        Ok(ViewAnswer::Building { retry_after }) => return Err(ViewWait::Building(retry_after)),
        Err(GitViewError::View(ViewError::NotARepository(_))) => {
            return Err(ViewWait::MirrorMissing);
        }
        Err(GitViewError::TooManyViews) => return Err(ViewWait::TooManyViews),
        Err(e) => {
            tracing::warn!(repo = %repo.name, error = %e, "syncing the git view");
            return Err(ViewWait::Failed("view_sync"));
        }
    }
    views.view(repo).map_err(|e| {
        tracing::warn!(repo = %repo.name, error = %e, "opening the git view");
        ViewWait::Failed("view_open")
    })
}

/// The protocol a request is served over: the client's, except that v2 is dropped where it cannot
/// or must not be used. The client falls back to v0 by itself.
///
/// * Git has no protocol v2 for pushes: a client that asks for it gets v0.
/// * A mirror holds its upstream's hidden refs (pull requests, ...) with their objects. In
///   protocol v2 a client may `want` any object that exists, whatever was advertised; v0 and v1
///   make git check each `want` against the visible refs. A mirror is therefore served over
///   v0/v1, and the answer is the same for every request of the repo. A derived view holds
///   shared refs only, so it has nothing to hide.
pub(crate) fn served_protocol(
    service: Service,
    mode: RepoMode,
    protocol: Option<GitProtocol>,
) -> Option<GitProtocol> {
    let v2 = protocol
        .as_ref()
        .is_some_and(|p| p.version() == ProtocolVersion::V2);
    if v2 && (service == Service::ReceivePack || mode == RepoMode::Mirror) {
        None
    } else {
        protocol
    }
}

/// [`served_protocol`] for a request whose view is known: a view that holds objects no shared
/// ref reaches (a push stored as a user branch left them in its object store) is also served over
/// v0/v1. Protocol v2 would let any reader `want` those objects; v0/v1 check each `want` against
/// the advertised refs, so only what the caller's visible refs reach can be fetched.
pub(crate) fn served_protocol_for(
    service: Service,
    mode: RepoMode,
    protocol: Option<GitProtocol>,
    unshared: bool,
) -> Option<GitProtocol> {
    let v2 = protocol
        .as_ref()
        .is_some_and(|p| p.version() == ProtocolVersion::V2);
    if v2 && unshared {
        None
    } else {
        served_protocol(service, mode, protocol)
    }
}

/// The git layer's parts, shared by every transport that serves it (smart HTTP, SSH).
#[derive(Clone)]
pub struct GitServices {
    pub(crate) views: Arc<GitViewService>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) metrics: Arc<GitMetrics>,
    pub(crate) limits: Arc<GitLimits>,
    /// Receive-pack: the hook socket, tokens and push slots. `None` refuses pushes.
    pub(crate) push: Option<GitPush>,
    pub(crate) usage: Arc<crate::usage::UsageSlot>,
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
    /// Receive-pack: the hook socket, tokens and push slots. `None` answers pushes with `403`.
    push: Option<GitPush>,
    /// Counts the bytes a response sends against the repository's bandwidth.
    usage: Arc<crate::usage::UsageSlot>,
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

    fn retry(
        status: StatusCode,
        reason: &'static str,
        after: Duration,
        body: &'static str,
    ) -> Self {
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

/// What a pack child reads on stdin.
enum Input {
    /// An advertisement reads nothing.
    None,
    /// A buffered fetch request.
    Bytes(Vec<u8>),
    /// A push: the first bytes already read and the rest of the body, streamed, at most `cap`.
    Body {
        first: Vec<u8>,
        stream: BodyDataStream,
        cap: u64,
    },
}

/// One run of `upload-pack` or `receive-pack`.
struct PackJob {
    service: Service,
    advertise: bool,
    protocol: Option<GitProtocol>,
    prefix: Vec<u8>,
    input: Input,
    content_type: String,
    push: Option<PushRun>,
}

/// What a push holds for as long as its child runs: the hook session (its token dies with it) and
/// the push slot.
struct PushRun {
    session: PushSession,
    _permit: PushPermit,
    entry: Arc<RepoEntry>,
}

/// The answer to the bare `0000` probe: an empty result, before any child starts.
fn probe_response(service: Service) -> Response {
    let mut res = Response::new(Body::empty());
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&result_type(service)) {
        h.insert(header::CONTENT_TYPE, v);
    }
    no_cache(h);
    res
}

/// The refusal for a body that could not be read.
fn body_refusal(f: BodyFail) -> Refusal {
    match f {
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
    }
}

/// The next chunk of a body within the idle limit and the deadline; `None` at its end.
async fn next_chunk(
    stream: &mut BodyDataStream,
    idle: Duration,
    deadline: tokio::time::Instant,
) -> Result<Option<Bytes>, Refusal> {
    let wait = (tokio::time::Instant::now() + idle).min(deadline);
    match tokio::time::timeout_at(wait, stream.next()).await {
        Err(_) => Err(body_refusal(BodyFail::Timeout)),
        Ok(None) => Ok(None),
        Ok(Some(Err(e))) => Err(body_refusal(if rate_limited(&e) {
            BodyFail::Limited
        } else {
            BodyFail::Broken
        })),
        Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
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
fn send_bounded(tx: &mpsc::Sender<Msg>, mut msg: Msg, idle: Duration, deadline: Instant) -> bool {
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

/// What feeds a child's stdin.
enum Feed {
    /// An advertisement reads nothing: closing stdin lets the child see its end.
    None,
    /// A buffered request (a fetch's negotiation).
    Bytes(Vec<u8>),
    /// A push: the chunks of the request body as they arrive.
    Stream(mpsc::Receiver<Bytes>),
}

/// Runs `child` on its own threads: one feeds `input` to its stdin, this one reads stdout to the
/// HTTP body through `tx`, then waits for it and calls `finish`.
fn drive(
    mut child: ServerChild,
    input: Feed,
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
            (Some(mut stdin), Feed::Bytes(body)) => {
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
            (Some(mut stdin), Feed::Stream(mut rx)) => {
                let act = activity.clone();
                std::thread::Builder::new()
                    .name("asc-git-http-in".into())
                    .spawn(move || {
                        while let Some(part) = rx.blocking_recv() {
                            if stdin.write_all(&part).is_err() {
                                // The child stopped reading: its exit status says why.
                                return;
                            }
                            act.touch();
                        }
                    })
                    .ok()
            }
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
                            // SIGTERM first: a git killed outright leaves its push quarantine.
                            child.terminate(Duration::from_secs(2));
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

/// What the body pump of a push learned, read by the audit record and the answer.
#[derive(Default)]
struct BodyState {
    /// Why the body was cut short; zero if it was not.
    cut: AtomicU8,
    /// Bytes of the request body read so far, counted as they arrive.
    bytes: AtomicU64,
    /// The pump has ended: the body is read, cut or abandoned.
    finished: std::sync::atomic::AtomicBool,
    /// Wakes the one waiter on `finished`.
    done: tokio::sync::Notify,
}

impl BodyState {
    /// Waits until the pump has ended. The pump is bounded by the idle and request deadlines, so
    /// this is too.
    async fn wait_finished(&self) {
        while !self.finished.load(Ordering::SeqCst) {
            self.done.notified().await;
        }
    }
}

/// Copies a push's request body to the child's stdin in order, counting bytes as they arrive (never
/// from `Content-Length`). A body that is over `cap`, stalls past `idle` or `deadline`, errs, or is
/// rate-limited ends the push: the reason is recorded in `state` and the child's input ends, so
/// git fails on the short pack and removes its quarantine itself. A request cut short is never
/// executed: git checks the framing of the command list, the options and the pack's own length
/// and checksum before it runs the hook.
async fn pump_body(
    first: Vec<u8>,
    stream: BodyDataStream,
    tx: mpsc::Sender<Bytes>,
    state: Arc<BodyState>,
    cap: u64,
    idle: Duration,
    deadline: tokio::time::Instant,
) {
    pump_body_inner(first, stream, tx, &state, cap, idle, deadline).await;
    state.finished.store(true, Ordering::SeqCst);
    state.done.notify_one();
}

/// A child that stops reading (git refused the pack at `receive.maxInputSize`, say) does not end
/// the pump: the rest of the body is still read and counted, never forwarded, up to `cap`, `idle`
/// and `deadline`. The client then gets the answer instead of a reset connection, and a body that
/// is over the cap is a `413` whatever git did first.
async fn pump_body_inner(
    first: Vec<u8>,
    mut stream: BodyDataStream,
    tx: mpsc::Sender<Bytes>,
    state: &BodyState,
    cap: u64,
    idle: Duration,
    deadline: tokio::time::Instant,
) {
    let mut child_gone = false;
    let stop = |why: u8| {
        state.cut.store(why, Ordering::SeqCst);
    };
    let mut total = first.len() as u64;
    state.bytes.store(total, Ordering::SeqCst);
    if total > cap {
        return stop(CUT_TOO_LARGE);
    }
    if !first.is_empty() && tx.send(Bytes::from(first)).await.is_err() {
        child_gone = true;
    }
    loop {
        let wait = (tokio::time::Instant::now() + idle).min(deadline);
        match tokio::time::timeout_at(wait, stream.next()).await {
            Err(_) => return stop(CUT_TIMEOUT),
            // The whole body arrived; dropping `tx` lets the child see the end of its input.
            Ok(None) => return,
            Ok(Some(Err(e))) => {
                return stop(if rate_limited(&e) {
                    CUT_LIMITED
                } else {
                    CUT_BROKEN
                });
            }
            Ok(Some(Ok(chunk))) => {
                total = total.saturating_add(chunk.len() as u64);
                state.bytes.store(total, Ordering::SeqCst);
                if total > cap {
                    return stop(CUT_TOO_LARGE);
                }
                if !child_gone && tx.send(chunk).await.is_err() {
                    // The child stopped reading; it ends on its own.
                    child_gone = true;
                }
            }
        }
    }
}

/// The status that a cut body maps to.
fn cut_status(cut: u8) -> Option<(StatusCode, &'static str, &'static str)> {
    match cut {
        CUT_TOO_LARGE => Some((
            StatusCode::PAYLOAD_TOO_LARGE,
            "push_size",
            "asc: the push is larger than this server accepts
",
        )),
        CUT_TIMEOUT => Some((
            StatusCode::REQUEST_TIMEOUT,
            "body_timeout",
            "asc: the request body stalled
",
        )),
        CUT_LIMITED => Some((
            StatusCode::TOO_MANY_REQUESTS,
            "rate",
            "asc: too many git requests; retry later
",
        )),
        CUT_BROKEN => Some((
            StatusCode::BAD_REQUEST,
            "body",
            "asc: the request body broke
",
        )),
        _ => None,
    }
}

/// The body of a streaming answer: `head` first, then what the reader thread sends. A child that
/// failed after the answer began ends the body with an error, so the client sees a broken
/// transfer and never a clean end. `finished` says the child already ended well: `head` is then the
/// whole body.
fn stream_body(head: Vec<Bytes>, rx: mpsc::Receiver<Msg>, finished: bool) -> Body {
    let state = (VecDeque::from(head), rx, finished);
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
            AuthError::Unauthenticated => {
                Refusal::empty(StatusCode::UNAUTHORIZED, "unauthenticated")
            }
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
    async fn ready_view(&self, repo: &Arc<RepoEntry>) -> Result<asc_git::compat::GitView, Refusal> {
        sync_view(&self.views, &self.registry, repo)
            .await
            .map_err(|wait| match wait {
                ViewWait::Building(retry_after) => Refusal::retry(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "view_building",
                    retry_after,
                    "asc: the repository is being prepared for git; retry shortly\n",
                ),
                ViewWait::MirrorMissing => Refusal::retry(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "mirror_missing",
                    NOT_READY_RETRY,
                    "asc: the mirror has not been fetched yet; retry shortly\n",
                ),
                ViewWait::TooManyViews => Refusal::retry(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too_many_views",
                    NOT_READY_RETRY,
                    "asc: the server is busy; retry shortly\n",
                ),
                ViewWait::Failed(reason) => Refusal::internal(reason),
            })
    }

    /// Starts `upload-pack` or `receive-pack` on the view and returns the streaming answer (or
    /// why it failed before its first byte).
    async fn stream_pack(
        &self,
        view: asc_git::compat::GitView,
        job: PackJob,
        audit: Audit,
    ) -> Result<Response, Refusal> {
        let PackJob {
            service,
            advertise,
            protocol,
            prefix,
            input,
            content_type,
            push,
        } = job;
        let req = PackRequest {
            advertise_refs: advertise,
            protocol,
        };
        let pack_service = match service {
            Service::UploadPack => PackService::UploadPack,
            Service::ReceivePack => PackService::ReceivePack,
        };
        let env: Vec<(&'static str, String)> = push
            .as_ref()
            .map(|p| p.session.env().map(|(k, v)| (k, v.to_owned())).to_vec())
            .unwrap_or_default();
        let spawned = tokio::task::spawn_blocking(move || {
            let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            view.spawn_pack_with_env(pack_service, &req, &env_ref)
        })
        .await;
        let child = match spawned {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::warn!(repo = %audit.repo, error = %e, "starting a git pack child");
                return Err(Refusal::internal("spawn"));
            }
            Err(_) => return Err(Refusal::internal("spawn")),
        };
        let mut body_state: Option<Arc<BodyState>> = None;
        let feed = match input {
            Input::None => Feed::None,
            Input::Bytes(b) => Feed::Bytes(b),
            Input::Body { first, stream, cap } => {
                let state = Arc::new(BodyState::default());
                body_state = Some(state.clone());
                let (btx, brx) = mpsc::channel::<Bytes>(QUEUE);
                let deadline = tokio::time::Instant::now() + self.limits.request_timeout;
                tokio::spawn(pump_body(
                    first,
                    stream,
                    btx,
                    state,
                    cap,
                    self.limits.idle_timeout,
                    deadline,
                ));
                Feed::Stream(brx)
            }
        };
        if let Some(p) = &push {
            // The hook keeps the idle watchdog fed while it imports: git itself is silent.
            p.session.set_activity(child.activity());
        }
        let (tx, mut rx) = mpsc::channel::<Msg>(QUEUE);
        let metrics = self.metrics.clone();
        let (service_name, version) = (audit.service, audit.version);
        let state = body_state.clone();
        let handle = tokio::runtime::Handle::current();
        let (views, registry) = (self.views.clone(), self.registry.clone());
        let meter_repo = audit.repo.clone();
        let finish: Finish = Box::new(move |report, sent_any, out_bytes| {
            let cut_by = state.as_ref().map_or(0, |s| s.cut.load(Ordering::SeqCst));
            let mut audit = audit;
            if let Some(s) = &state {
                audit.bytes_in = s.bytes.load(Ordering::SeqCst);
            }
            let status = if let Some((status, _, _)) = cut_status(cut_by) {
                status
            } else if sent_any {
                StatusCode::OK
            } else if report.killed.is_some() {
                StatusCode::GATEWAY_TIMEOUT
            } else if report.ok {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            let mut outcome = match (report.killed, report.ok) {
                (Some(KillReason::Deadline | KillReason::Idle), _) => "killed".to_owned(),
                (Some(KillReason::Requested), _) => "limit:client_gone".to_owned(),
                (None, true) => "ok".to_owned(),
                (None, false) => "rejected:child_failed".to_owned(),
            };
            if let Some((_, reason, _)) = cut_status(cut_by) {
                outcome = if cut_by == CUT_TOO_LARGE {
                    "limit:push_bytes".to_owned()
                } else {
                    format!("rejected:{reason}")
                };
            }
            if let Some(p) = &push {
                // What the hook decided is more precise than how the child ended.
                let done = p.session.outcome();
                audit.objects = done.objects;
                let hook_ran = done.result != "rejected:no_hook";
                if hook_ran && cut_by == 0 && report.killed.is_none() {
                    outcome.clone_from(&done.result);
                } else if !hook_ran && report.ok && cut_by == 0 {
                    outcome = "noop".to_owned();
                }
                if done.result == "ok" {
                    audit.refs = done
                        .refs
                        .iter()
                        .map(|r| {
                            format!(
                                "{} {} {}",
                                r.name,
                                r.old.map_or_else(|| "-".to_owned(), |o| o.to_string()),
                                r.new.map_or_else(|| "-".to_owned(), |o| o.to_string()),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    // asc's refs are committed; bring the view in step (a user ref or a ref that
                    // git set differently is removed or reset) so the next fetch finds it ready.
                    let (views, registry, entry) =
                        (views.clone(), registry.clone(), p.entry.clone());
                    // Refs git wrote for a private branch stay hidden from readers until the
                    // sync below has removed them.
                    let hidden = p.session.take_hidden();
                    handle.spawn(async move {
                        match views.ensure(&entry, registry.refs()).await {
                            Ok(crate::git_view::ViewAnswer::Ready(_)) => {
                                // The run may have started before the push wrote its refs
                                // (coalesced); only names the view lost are shown again.
                                if let Some(h) = hidden {
                                    h.release_if_gone();
                                }
                            }
                            other => {
                                if let Err(e) = other {
                                    tracing::warn!(repo = %entry.name, error = %e, "syncing the git view after a push");
                                }
                                // The private refs are still in the view: they stay hidden until
                                // a later sync removes them.
                                if let Some(h) = hidden {
                                    h.until_next_sync();
                                }
                            }
                        }
                    });
                }
                if outcome.starts_with("limit:") {
                    metrics.reject("limit");
                }
            }
            metrics.request(service_name, version, status);
            metrics.bytes("out", out_bytes);
            metrics.bytes("in", audit.bytes_in);
            metrics.child_seconds.observe(report.elapsed.as_secs_f64());
            if report.killed.is_some() {
                metrics.reject("killed");
            }
            audit.emit(&outcome, status, out_bytes);
        });
        drive(child, feed, tx, self.limits.clone(), finish);

        let first = rx.recv().await;
        let mut finished = false;
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
                finished = true;
                if prefix.is_empty() {
                    Vec::new()
                } else {
                    vec![Bytes::from(prefix)]
                }
            }
            Some(Msg::End(report)) => {
                // A child that ended early leaves the rest of the body to the pump, which may
                // still find it over the cap.
                if let Some(s) = &body_state {
                    s.wait_finished().await;
                }
                let cut_by = body_state
                    .as_ref()
                    .map_or(0, |s| s.cut.load(Ordering::SeqCst));
                let mut refusal = if let Some((status, reason, body)) = cut_status(cut_by) {
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        Refusal::retry(status, reason, Duration::from_secs(1), body)
                    } else {
                        Refusal::plain(status, reason, body)
                    }
                } else {
                    match report.killed {
                        Some(KillReason::Deadline | KillReason::Idle) => Refusal::plain(
                            StatusCode::GATEWAY_TIMEOUT,
                            "timeout",
                            "asc: the git request took too long
",
                        ),
                        _ => Refusal::internal("child_failed"),
                    }
                };
                refusal.recorded = true;
                return Err(refusal);
            }
            None => return Err(Refusal::internal("child_lost")),
        };
        let body = stream_body(head, rx, finished);
        let body = match self.usage.get() {
            Some(hub) => Body::from_stream(crate::usage::meter::Metered::new(
                Box::pin(body.into_data_stream()),
                hub.clone(),
                meter_repo,
                |r: &Result<Bytes, axum::Error>| r.as_ref().map_or(0, |b| b.len() as u64),
            )),
            None => body,
        };
        let mut response = Response::new(body);
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

        let mut protocol = protocol_of(&parts.headers);
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
            if self.push.is_none() {
                return Err(Refusal::plain(
                    StatusCode::FORBIDDEN,
                    "push_unavailable",
                    PUSH_DISABLED,
                ));
            }
        }
        // Whether the view holds unshared objects is asked only where a v2 answer could expose
        // them (an upload-pack advertisement or request), and not for the bare `0000` probe, which
        // opens no view. Until then the version is the one the repo mode allows.
        protocol = served_protocol(service, entry.mode, protocol);
        audit.version = version_label(protocol.as_ref());

        match endpoint {
            Endpoint::InfoRefs => {
                // A push advertisement: the same disk and view gates as the push itself would
                // meet are not applied here (discovery is cheap); the view must be ready.
                protocol = self.unshared_aware(&entry, service, protocol);
                audit.version = version_label(protocol.as_ref());
                let view = self.ready_view(&entry).await?;
                let prefix = advertisement_prefix(service, protocol.as_ref());
                let response = self
                    .stream_pack(
                        view,
                        PackJob {
                            service,
                            advertise: true,
                            protocol,
                            prefix,
                            input: Input::None,
                            content_type: advertisement_type(service),
                            push: None,
                        },
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
                let encoding = body_encoding(header_str(&parts.headers, header::CONTENT_ENCODING))
                    .map_err(|_| {
                        Refusal::plain(
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            "content_encoding",
                            "asc: unsupported content encoding\n",
                        )
                    })?;
                let cap = self.limits.fetch_body_bytes;
                let deadline = tokio::time::Instant::now() + self.limits.request_timeout;
                let raw = read_body(body, cap, self.limits.idle_timeout, deadline)
                    .await
                    .map_err(body_refusal)?;
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
                        return Ok(Reply::Done(probe_response(Service::UploadPack)));
                    }
                    UploadBody::Request(b) => b,
                };
                protocol = self.unshared_aware(&entry, service, protocol);
                audit.version = version_label(protocol.as_ref());
                let view = self.ready_view(&entry).await?;
                let response = self
                    .stream_pack(
                        view,
                        PackJob {
                            service,
                            advertise: false,
                            protocol,
                            prefix: Vec::new(),
                            input: Input::Bytes(input),
                            content_type: result_type(Service::UploadPack),
                            push: None,
                        },
                        audit.clone(),
                    )
                    .await?;
                Ok(Reply::Streaming(response))
            }
            Endpoint::ReceivePack => {
                self.receive(&entry, &principal, &parts.headers, body, protocol, audit)
                    .await
            }
        }
    }

    /// `protocol` after the unshared-objects rule ([`served_protocol_for`]). A view that cannot be
    /// opened is treated as holding unshared objects; the request then fails in `ready_view` with
    /// the proper answer. Only a v2 request needs the view, so any other opens none here.
    fn unshared_aware(
        &self,
        entry: &Arc<RepoEntry>,
        service: Service,
        protocol: Option<GitProtocol>,
    ) -> Option<GitProtocol> {
        if protocol
            .as_ref()
            .is_none_or(|p| p.version() != ProtocolVersion::V2)
        {
            return protocol;
        }
        let unshared = self.views.view(entry).map_or(true, |v| v.holds_unshared());
        served_protocol_for(service, entry.mode, protocol, unshared)
    }

    /// `POST git-receive-pack`: the probe, the push slot and disk gates, then the push streamed to
    /// `git receive-pack --stateless-rpc` with a hook session (SPEC §25.2, §25.5).
    async fn receive(
        &self,
        entry: &Arc<RepoEntry>,
        principal: &Principal,
        headers: &HeaderMap,
        body: Body,
        protocol: Option<GitProtocol>,
        audit: &mut Audit,
    ) -> Result<Reply, Refusal> {
        let Some(push) = self.push.clone() else {
            return Err(Refusal::internal("receive_pack"));
        };
        let wanted = request_type(Service::ReceivePack);
        if !content_type_matches(header_str(headers, header::CONTENT_TYPE), &wanted) {
            return Err(Refusal::plain(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content_type",
                "asc: unsupported content type\n",
            ));
        }
        // Pushes are sent as they are (git compresses nothing around the pack); a transformed
        // body would also defeat the byte count that bounds the push.
        if !matches!(
            body_encoding(header_str(headers, header::CONTENT_ENCODING)),
            Ok(BodyEncoding::Identity)
        ) {
            return Err(Refusal::plain(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content_encoding",
                "asc: unsupported content encoding\n",
            ));
        }

        // The head of the body decides between the probe and a push: read until four bytes are
        // known, then, if they are `0000`, whether anything follows.
        let mut stream = body.into_data_stream();
        let (idle, deadline) = (
            self.limits.idle_timeout,
            tokio::time::Instant::now() + self.limits.request_timeout,
        );
        let mut first: Vec<u8> = Vec::new();
        let mut ended = false;
        while first.len() < 4 && !ended {
            match next_chunk(&mut stream, idle, deadline).await? {
                Some(chunk) => first.extend_from_slice(&chunk),
                None => ended = true,
            }
        }
        if first == pktline::FLUSH && !ended {
            match next_chunk(&mut stream, idle, deadline).await? {
                Some(chunk) => first.extend_from_slice(&chunk),
                None => ended = true,
            }
        }
        if first.is_empty() {
            return Err(Refusal::empty(StatusCode::BAD_REQUEST, "body"));
        }
        if ended && first == pktline::FLUSH {
            // Answered before any child starts (SPEC §25.5).
            return Ok(Reply::Done(probe_response(Service::ReceivePack)));
        }

        // A push: a slot, then the disk, then the view.
        let permit = push.acquire(&principal.id).map_err(|busy| match busy {
            Busy::Principal => Refusal::retry(
                StatusCode::TOO_MANY_REQUESTS,
                "push_slots_principal",
                PUSH_RETRY,
                PUSH_BUSY_PRINCIPAL,
            ),
            Busy::Replica => Refusal::retry(
                StatusCode::SERVICE_UNAVAILABLE,
                "push_slots_replica",
                PUSH_RETRY,
                PUSH_BUSY_REPLICA,
            ),
        })?;
        if !push.disk_ok() {
            return Err(Refusal::retry(
                StatusCode::SERVICE_UNAVAILABLE,
                "disk_low",
                PUSH_RETRY,
                DISK_LOW,
            ));
        }
        let view = self.ready_view(entry).await?;
        let ctx = CallCtx {
            repo: entry.name.clone(),
            id_mode: IdMode::Asc,
            principal: principal.clone(),
            op_id: Some(audit.op_id),
            client: None,
            admin: Default::default(),
        };
        let session = push.begin(ctx, entry.clone(), view.clone()).map_err(|e| {
            tracing::warn!(error = %e, "starting a push session");
            Refusal::retry(
                StatusCode::SERVICE_UNAVAILABLE,
                "push_sessions",
                PUSH_RETRY,
                PUSH_BUSY_REPLICA,
            )
        })?;
        let cap = self.limits.max_push_bytes.saturating_add(PUSH_BODY_SLACK);
        let response = self
            .stream_pack(
                view,
                PackJob {
                    service: Service::ReceivePack,
                    advertise: false,
                    protocol,
                    prefix: Vec::new(),
                    input: Input::Body { first, stream, cap },
                    content_type: result_type(Service::ReceivePack),
                    push: Some(PushRun {
                        session,
                        _permit: permit,
                        entry: entry.clone(),
                    }),
                },
                audit.clone(),
            )
            .await?;
        Ok(Reply::Streaming(response))
    }

    async fn handle(
        &self,
        endpoint: Endpoint,
        owner: String,
        repo: String,
        req: Request,
    ) -> Response {
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
            objects: 0,
            refs: String::new(),
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
                    audit.emit(&format!("rejected:{}", refusal.reason), refusal.status, 0);
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
    cfg: &ServeConfig,
) -> GitServices {
    match HookProgram::current_asc() {
        Ok(hook) => register_with_hook(ext, views, registry, metrics, cfg, &hook),
        Err(e) => {
            // Fetches still work; pushes answer `403` (the hook has no program to run).
            tracing::warn!(error = %e, "cannot find this executable; git pushes are disabled");
            register_routes(ext, views, registry, metrics, None)
        }
    }
}

/// [`register`] with the program the `pre-receive` hook runs: `asc serve git-hook pre-receive` for
/// the real server, the test binary in the tests of this crate.
pub fn register_with_hook(
    ext: &Extensions,
    views: Arc<GitViewService>,
    registry: Arc<Registry>,
    metrics: &Arc<Metrics>,
    _cfg: &ServeConfig,
    hook: &HookProgram,
) -> GitServices {
    let push = match GitPush::start(views.clone(), registry.clone(), ext.pipeline.clone(), hook) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "the git push layer did not start; pushes are disabled");
            None
        }
    };
    register_routes(ext, views, registry, metrics, push)
}

fn register_routes(
    ext: &Extensions,
    views: Arc<GitViewService>,
    registry: Arc<Registry>,
    metrics: &Arc<Metrics>,
    push: Option<GitPush>,
) -> GitServices {
    let limits = Arc::new(views.server().limits().clone());
    let metrics = GitMetrics::register(metrics);
    let services = GitServices {
        views,
        registry,
        metrics,
        limits,
        push,
        usage: ext.usage.clone(),
    };
    let shared = services.clone();
    let factory: GitRouterFactory = Arc::new(move |authorizer| {
        router(GitHttp {
            views: shared.views.clone(),
            registry: shared.registry.clone(),
            authorizer,
            metrics: shared.metrics.clone(),
            limits: shared.limits.clone(),
            push: shared.push.clone(),
            usage: shared.usage.clone(),
        })
    });
    if ext.git.set(factory).is_err() {
        tracing::warn!("the git routes were already registered; keeping the first");
    }
    services
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2() -> Option<GitProtocol> {
        GitProtocol::parse("version=2")
    }

    #[test]
    fn git_http_v2_is_dropped_for_a_view_holding_unshared_objects() {
        let kept = served_protocol_for(Service::UploadPack, RepoMode::Primary, v2(), false);
        assert_eq!(kept.map(|p| p.version()), Some(ProtocolVersion::V2));
        let dropped = served_protocol_for(Service::UploadPack, RepoMode::Primary, v2(), true);
        assert!(dropped.is_none());
    }
}
