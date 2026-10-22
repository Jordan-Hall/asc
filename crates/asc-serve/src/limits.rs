//! Admission control for `asc serve` (SPEC §16.3, §16.4): per-principal rate classes, in-flight
//! caps, load shedding, and the `limits.text_memory` byte semaphore.
//!
//! Nothing here queues. A request over a limit is refused at once with a `retry-after-ms`
//! trailer; only [`TextMemory`] waits, and for at most one second.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::{Body, Bytes, HttpBody};
use axum::http::{Request, Response};
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::errors::RpcError;
use asc_rpc::limits::{CounterGuard, KeyedCounter, KeyedLimiter, MAX_KEYS, OnFull, Shed};
use asc_rpc::retry::RETRY_AFTER_MS;
use asc_rpc::server::rpc_status;
use futures::future::BoxFuture;
use http_body::Frame;
use tokio::sync::Semaphore;
use tonic::Status;
use tonic::transport::server::TcpConnectInfo;
use tower::{Layer, Service};

use crate::config::Limits;
use crate::{TextBudget, TextBudgetExhausted, TextGuard};

/// How long a request may wait for text memory before it is shed (SPEC §16.4).
pub const TEXT_WAIT: Duration = Duration::from_secs(1);
/// Text-memory accounting granularity.
const TEXT_UNIT: u64 = 1024;
/// Retry hint for a refused in-flight slot.
const INFLIGHT_RETRY: Duration = Duration::from_millis(100);
/// Background work is shed once less than this share of `max_inflight` is free.
const BACKGROUND_FREE_SHARE: u32 = 4;

// ------------------------------------------------------------------------------ text memory

/// The per-replica budget of whole-file buffer bytes (`limits.text_memory`).
pub struct TextMemory {
    sem: Arc<Semaphore>,
    units: u32,
    wait: Duration,
}

impl TextMemory {
    /// A budget of `bytes` (rounded down to 1 KiB) that makes callers wait at most [`TEXT_WAIT`].
    pub fn new(bytes: u64) -> Self {
        Self::with_wait(bytes, TEXT_WAIT)
    }

    pub fn with_wait(bytes: u64, wait: Duration) -> Self {
        let units = u32::try_from(bytes / TEXT_UNIT).unwrap_or(u32::MAX).max(1);
        Self {
            sem: Arc::new(Semaphore::new(units as usize)),
            units,
            wait,
        }
    }

    /// Bytes not currently held.
    pub fn available(&self) -> u64 {
        self.sem.available_permits() as u64 * TEXT_UNIT
    }
}

#[async_trait]
impl TextBudget for TextMemory {
    async fn acquire(&self, bytes: u64) -> Result<TextGuard, TextBudgetExhausted> {
        let want = bytes.div_ceil(TEXT_UNIT).max(1);
        // A request larger than the whole budget could never be granted.
        let Ok(want) = u32::try_from(want) else {
            return Err(TextBudgetExhausted);
        };
        if want > self.units {
            return Err(TextBudgetExhausted);
        }
        match tokio::time::timeout(self.wait, self.sem.clone().acquire_many_owned(want)).await {
            Ok(Ok(permit)) => Ok(TextGuard::holding(permit)),
            _ => Err(TextBudgetExhausted),
        }
    }
}

impl From<TextBudgetExhausted> for Status {
    fn from(_: TextBudgetExhausted) -> Self {
        let mut s = Status::resource_exhausted("text memory budget exhausted");
        set_retry_after(&mut s, INFLIGHT_RETRY);
        s
    }
}

// ------------------------------------------------------------------------------ classes

/// The SPEC §16.3 rate class of an RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    Cheap,
    Objects,
    Expensive,
    UpdateRefs,
    Codeintel,
    Semantic,
    /// Cloud uploads and ingest; shed first under load.
    Background,
    /// Forgejo's internal token and the CI warmer.
    Service,
}

/// The class of a gRPC call, or `None` for a path this server does not rate-limit.
/// An unlisted method of a known service is `Expensive`, so a new RPC fails safe.
pub fn class_of(path: &str) -> Option<Class> {
    let (service, method) = path.strip_prefix("/asc.v1.")?.split_once('/')?;
    Some(match (service, method) {
        ("Repo", "GetObjects" | "PutObjects" | "ReadFile") => Class::Objects,
        ("Repo", "UpdateRefs") => Class::UpdateRefs,
        ("Repo", "Ingest") => Class::Background,
        (
            "Repo",
            "Version" | "ListRefs" | "Resolve" | "GitMap" | "HasObjects" | "Stat" | "MergeBase"
            | "IsAncestor" | "ReadChangeset" | "FindLanding" | "RepoConfig" | "CheckAttr"
            | "LastCommits" | "ReadTree" | "Compare",
        ) => Class::Cheap,
        ("Repo", _) => Class::Expensive,
        ("Changes", "ResolveChanges" | "Stack" | "Versions" | "Overlaps" | "SetReviewState") => {
            Class::Cheap
        }
        ("Changes", _) => Class::Expensive,
        ("Cloud", "GetWorkspace" | "ListWorkspaces") => Class::Cheap,
        ("Cloud", _) => Class::Background,
        ("CodeIntel", "Search" | "Symbols") => Class::Codeintel,
        ("CodeIntel", "Semantic") => Class::Semantic,
        ("CodeIntel", "IndexStatus") => Class::Cheap,
        ("CodeIntel", _) => Class::Expensive,
        ("Projects", "ProjectGraph" | "ProjectHashes") => Class::Cheap,
        ("Projects", _) => Class::Expensive,
        _ => return None,
    })
}

/// How long an authorizer-verified token keeps its own bucket after the authorizer last
/// accepted it (the positive cache lifetime of SPEC §15.1).
const VERIFIED_TTL: Duration = Duration::from_secs(60);
/// Verified tokens remembered at once. Past this, a newly verified token stays in the peer
/// tier until an entry expires.
const MAX_VERIFIED: usize = 100_000;

/// The network part of an unverified caller. IPv4 is keyed by address, IPv6 by its /64, so a
/// client that owns a prefix cannot mint a fresh bucket per address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Peer {
    V4([u8; 4]),
    V6([u8; 8]),
    /// No peer address is known; all such callers share one bucket.
    Unknown,
}

impl Peer {
    pub fn from_ip(ip: Option<IpAddr>) -> Self {
        match ip {
            None => Self::Unknown,
            Some(IpAddr::V4(a)) => Self::V4(a.octets()),
            Some(IpAddr::V6(a)) => match a.to_ipv4_mapped() {
                Some(v4) => Self::V4(v4.octets()),
                None => {
                    let o = a.octets();
                    Self::V6([o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]])
                }
            },
        }
    }
}

/// Who admission charges, as far as it can tell before authorisation (SPEC §16.3).
///
/// Admission runs before authentication, so a bearer token in a request proves nothing: a client
/// can send a fresh random one every time. Only a principal the authorizer has accepted
/// ([`Verified`]) or a configured service token gets a bucket of its own; everyone else is
/// charged to the network peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Caller {
    /// A principal the authorizer accepted (a hash of its id).
    Verified([u8; 32]),
    /// Anonymous, or a token the authorizer has not (recently) accepted.
    Unverified(Peer),
    /// A configured service token (Forgejo, the CI warmer).
    Service,
}

impl Caller {
    fn principal(id: &str) -> Self {
        Self::Verified(*blake3::hash(id.as_bytes()).as_bytes())
    }
}

fn token_hash(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

/// The `Bearer` token of a request, if it has one. Mirrors `AuthInterceptor`: the scheme is
/// case-insensitive.
fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let (scheme, rest) = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(rest)
}

/// The tokens the authorizer has accepted recently, and the principal each belongs to.
///
/// Only [`RecordingAuthorizer`] writes it, and only after a real authorisation succeeded, so an
/// attacker cannot add entries with made-up tokens. It is bounded: when full of live entries,
/// new tokens are simply not recorded and stay in the peer tier.
pub struct Verified {
    ttl: Duration,
    max: usize,
    inner: Mutex<VerifiedMap>,
}

#[derive(Default)]
struct VerifiedMap {
    tokens: HashMap<[u8; 32], (Instant, Caller)>,
    last_prune: Option<Instant>,
}

impl Verified {
    fn new(ttl: Duration, max: usize) -> Self {
        Self {
            ttl,
            max,
            inner: Mutex::default(),
        }
    }

    fn record(&self, token: &str, principal: &str) {
        let now = Instant::now();
        let key = token_hash(token);
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.tokens.len() >= self.max && !g.tokens.contains_key(&key) {
            let due = g
                .last_prune
                .is_none_or(|p| now.saturating_duration_since(p) >= Duration::from_secs(1));
            if due {
                g.last_prune = Some(now);
                let ttl = self.ttl;
                g.tokens
                    .retain(|_, (at, _)| now.saturating_duration_since(*at) < ttl);
            }
            if g.tokens.len() >= self.max {
                return;
            }
        }
        g.tokens.insert(key, (now, Caller::principal(principal)));
    }

    fn lookup(&self, token_hash: &[u8; 32]) -> Option<Caller> {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (at, who) = g.tokens.get(token_hash)?;
        (at.elapsed() < self.ttl).then_some(*who)
    }
}

/// An [`Authorizer`] that tells admission which tokens were accepted, so that those callers are
/// charged by principal and not by network peer.
pub struct RecordingAuthorizer {
    inner: Arc<dyn Authorizer>,
    verified: Arc<Verified>,
}

#[async_trait]
impl Authorizer for RecordingAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let answer = self.inner.authorize(token, repo, access).await;
        if let (Some(t), Ok(p)) = (token, &answer) {
            self.verified.record(t.expose(), &p.id);
        }
        answer
    }
}

/// A per-class limiter in two tiers, so that anonymous traffic cannot push out a verified caller.
/// Verified principals are costly to forge, so a full table evicts its least recently seen
/// bucket. Peers are cheap to rotate, so a full table sends newcomers to one shared bucket.
struct Tiered {
    verified: KeyedLimiter<Caller>,
    unverified: KeyedLimiter<Caller>,
}

impl Tiered {
    fn new(rate: u64, burst: u64, max_keys: usize) -> Self {
        Self {
            verified: KeyedLimiter::bounded(rate, burst, max_keys, OnFull::EvictOldest),
            unverified: KeyedLimiter::bounded(rate, burst, max_keys, OnFull::Overflow),
        }
    }

    fn tier(&self, who: &Caller) -> &KeyedLimiter<Caller> {
        match who {
            Caller::Unverified(_) => &self.unverified,
            Caller::Verified(_) | Caller::Service => &self.verified,
        }
    }

    fn check(&self, who: &Caller, cost: u64) -> Result<(), Shed> {
        self.tier(who).check(who, cost)
    }

    fn debit(&self, who: &Caller, cost: u64) {
        self.tier(who).debit(who, cost);
    }
}

// ------------------------------------------------------------------------------ admission

fn set_retry_after(status: &mut Status, after: Duration) {
    let ms = after.as_millis().max(1).to_string();
    if let Ok(v) = ms.parse() {
        status.metadata_mut().insert(RETRY_AFTER_MS, v);
    }
}

fn rate_limited(shed: Shed) -> Status {
    let mut s = rpc_status(&RpcError::RateLimited(Default::default()));
    set_retry_after(&mut s, shed.retry_after);
    s
}

fn overloaded() -> Status {
    let mut s = Status::unavailable("server overloaded");
    set_retry_after(&mut s, INFLIGHT_RETRY);
    s
}

/// Everything a request holds while it runs; dropping it frees the slots.
#[derive(Default)]
pub struct Permit(#[allow(dead_code)] Vec<Box<dyn Send + Sync>>);

impl Permit {
    fn hold(&mut self, part: impl Send + Sync + 'static) {
        self.0.push(Box::new(part));
    }
}

/// The replica's admission controller, built from `[limits]`.
pub struct Admission {
    cfg: Limits,
    inflight: Arc<Semaphore>,
    per_token: Arc<KeyedCounter<Caller>>,
    cheap: Tiered,
    objects: Arc<Tiered>,
    update_refs: Tiered,
    codeintel: Tiered,
    semantic: Tiered,
    service: Tiered,
    expensive_principal: Arc<KeyedCounter<Caller>>,
    expensive_replica: Arc<Semaphore>,
    semantic_replica: Arc<Semaphore>,
    service_tokens: HashSet<[u8; 32]>,
    verified: Arc<Verified>,
}

impl Admission {
    pub fn new(cfg: &Limits) -> Self {
        Self::with_table_size(cfg, MAX_KEYS)
    }

    /// As [`Admission::new`], with at most `max_keys` buckets per class and tier.
    pub fn with_table_size(cfg: &Limits, max_keys: usize) -> Self {
        let rate = |n: u32| u64::from(n);
        let tier = |r: u64, b: u64| Tiered::new(r, b, max_keys);
        Self {
            cfg: cfg.clone(),
            inflight: Arc::new(Semaphore::new(cfg.max_inflight as usize)),
            per_token: Arc::new(KeyedCounter::new()),
            cheap: tier(rate(cfg.cheap_rps), rate(cfg.cheap_burst)),
            objects: Arc::new(tier(
                cfg.objects_bytes_per_sec.0,
                cfg.objects_bytes_per_sec.0,
            )),
            // The burst is one second of the per-repo rate; the key is the caller (see `admit`).
            update_refs: tier(
                rate(cfg.update_refs_rps_per_repo),
                rate(cfg.update_refs_rps_per_repo),
            ),
            codeintel: tier(rate(cfg.codeintel_rps), rate(cfg.codeintel_burst)),
            semantic: tier(rate(cfg.semantic_rps), rate(cfg.semantic_rps)),
            service: tier(rate(cfg.service_rps), rate(cfg.service_burst)),
            expensive_principal: Arc::new(KeyedCounter::new()),
            expensive_replica: Arc::new(Semaphore::new(cfg.expensive_per_replica as usize)),
            semantic_replica: Arc::new(Semaphore::new(cfg.semantic_concurrency as usize)),
            service_tokens: HashSet::new(),
            verified: Arc::new(Verified::new(VERIFIED_TTL, MAX_VERIFIED)),
        }
    }

    /// Treats calls bearing `token` as the `service` class (Forgejo, the CI warmer).
    pub fn with_service_token(mut self, token: &str) -> Self {
        self.service_tokens.insert(token_hash(token));
        self
    }

    /// Wraps `inner` so that every token it accepts gets its own bucket here (see [`Caller`]).
    /// The wrapped authorizer must be the one that serves the gRPC routes this admission guards.
    pub fn recording(&self, inner: Arc<dyn Authorizer>) -> Arc<dyn Authorizer> {
        Arc::new(RecordingAuthorizer {
            inner,
            verified: self.verified.clone(),
        })
    }

    /// Who to charge for a request with this bearer `token` (if any) from `peer`.
    pub fn identify(&self, token: Option<&str>, peer: Option<IpAddr>) -> Caller {
        if let Some(t) = token {
            let h = token_hash(t);
            if self.service_tokens.contains(&h) {
                return Caller::Service;
            }
            if let Some(who) = self.verified.lookup(&h) {
                return who;
            }
        }
        Caller::Unverified(Peer::from_ip(peer))
    }

    /// Requests currently admitted on this replica.
    pub fn in_flight(&self) -> usize {
        self.cfg.max_inflight as usize - self.inflight.available_permits()
    }

    /// Admits one call of `class` by `who`, charging `cost` (bytes for `Objects`, else ignored).
    /// Never waits: the error is the status to send back.
    pub fn admit(&self, who: Caller, class: Class, cost: u64) -> Result<Permit, Status> {
        let mut permit = Permit::default();
        let slot = self
            .inflight
            .clone()
            .try_acquire_owned()
            .map_err(|_| overloaded())?;
        permit.hold(slot);
        let max = self.cfg.max_inflight;
        if class == Class::Background
            && self.inflight.available_permits() < (max / BACKGROUND_FREE_SHARE) as usize
        {
            return Err(overloaded());
        }
        let token_slot = self
            .per_token
            .try_acquire(&who, self.cfg.max_inflight_per_token)
            .map_err(rate_limited)?;
        permit.hold(CounterHold(token_slot));

        let class = if who == Caller::Service {
            Class::Service
        } else {
            class
        };
        match class {
            Class::Cheap | Class::Background => self.cheap.check(&who, 1),
            // `cost` is what is known up front; the body's bytes are charged as they stream.
            Class::Objects => self.objects.check(&who, cost),
            Class::UpdateRefs => self.update_refs.check(&who, 1),
            Class::Codeintel => self.codeintel.check(&who, 1),
            Class::Service => self.service.check(&who, 1),
            Class::Semantic => self.semantic.check(&who, 1),
            Class::Expensive => Ok(()),
        }
        .map_err(rate_limited)?;

        match class {
            Class::Expensive => {
                permit.hold(CounterHold(
                    self.expensive_principal
                        .try_acquire(&who, self.cfg.expensive_per_principal)
                        .map_err(rate_limited)?,
                ));
                permit.hold(self.replica_slot(&self.expensive_replica)?);
            }
            Class::Semantic => permit.hold(self.replica_slot(&self.semantic_replica)?),
            _ => {}
        }
        Ok(permit)
    }

    /// The meter for the body bytes of an `objects` call, or `None` for any other call.
    fn meter(&self, who: Caller, class: Class) -> Option<Meter> {
        (class == Class::Objects && who != Caller::Service).then(|| Meter {
            limiter: self.objects.clone(),
            who,
        })
    }

    fn replica_slot(
        &self,
        sem: &Arc<Semaphore>,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        sem.clone().try_acquire_owned().map_err(|_| overloaded())
    }
}

/// Lets a [`CounterGuard`] sit in a [`Permit`].
struct CounterHold(#[allow(dead_code)] CounterGuard<Caller>);

// ------------------------------------------------------------------------------ tower layer

/// Applies [`Admission`] to the gRPC calls of the `asc.v1` services; other routes pass through.
#[derive(Clone)]
pub struct AdmissionLayer {
    admission: Arc<Admission>,
}

impl AdmissionLayer {
    pub fn new(admission: Arc<Admission>) -> Self {
        Self { admission }
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = AdmissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AdmissionService {
            inner,
            admission: self.admission.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AdmissionService<S> {
    inner: S,
    admission: Arc<Admission>,
}

/// Charges the bytes of one `objects` call to its caller as they stream, in both directions.
/// The `Content-Length` header is not consulted: a client can omit it or lie.
#[derive(Clone)]
struct Meter {
    limiter: Arc<Tiered>,
    who: Caller,
}

type BodyError = Box<dyn std::error::Error + Send + Sync>;

/// A request body that takes each data frame from the caller's byte bucket before passing it on.
/// Over the limit it fails with `RESOURCE_EXHAUSTED` and ends, which stops an upload midway.
struct MeteredRequest {
    inner: Body,
    meter: Meter,
    failed: bool,
}

impl HttpBody for MeteredRequest {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref()
                    && let Err(shed) = self.meter.limiter.check(&self.meter.who, data.len() as u64)
                {
                    self.failed = true;
                    return Poll::Ready(Some(Err(Box::new(rate_limited(shed)))));
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.failed || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// A response body that keeps the request's [`Permit`] until the stream ends or is dropped. For
/// an `objects` call it also charges the bytes it sends. They exist already, so they overdraw
/// the bucket instead of failing; the caller's next calls wait until the debt is repaid.
struct Held {
    inner: Body,
    meter: Option<Meter>,
    _permit: Permit,
}

impl HttpBody for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let (Poll::Ready(Some(Ok(frame))), Some(m)) = (&polled, &self.meter)
            && let Some(data) = frame.data_ref()
        {
            m.limiter.debit(&m.who, data.len() as u64);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl<S> Service<Request<Body>> for AdmissionService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response<Body>, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let Some(class) = class_of(req.uri().path()) else {
            return Box::pin(inner.call(req));
        };
        // tonic puts the peer address on every request, plain TCP and TLS alike.
        let peer = req
            .extensions()
            .get::<TcpConnectInfo>()
            .and_then(TcpConnectInfo::remote_addr)
            .map(|a| a.ip());
        let who = self.admission.identify(bearer(req.headers()), peer);
        // The size of the body is not known yet and the header cannot be trusted, so the call
        // costs one unit here and the body's real bytes are charged as they stream.
        match self.admission.admit(who, class, 1) {
            Err(status) => Box::pin(std::future::ready(Ok(status.into_http()))),
            Ok(permit) => {
                let meter = self.admission.meter(who, class);
                let req = match &meter {
                    Some(m) => req.map(|inner| {
                        Body::new(MeteredRequest {
                            inner,
                            meter: m.clone(),
                            failed: false,
                        })
                    }),
                    None => req,
                };
                Box::pin(async move {
                    let resp = inner.call(req).await?;
                    Ok(resp.map(|body| {
                        Body::new(Held {
                            inner: body,
                            meter,
                            _permit: permit,
                        })
                    }))
                })
            }
        }
    }
}

/// The public routes of `ext`, behind `admission`. This is the only place that wires admission
/// into the server: it hands `authorizer` to the services through [`Admission::recording`] (so
/// that accepted tokens get their own bucket) and wraps the routes in the [`AdmissionLayer`] that
/// reads them.
pub fn guarded_routes(
    ext: &crate::Extensions,
    authorizer: Arc<dyn Authorizer>,
    admission: Arc<Admission>,
    info: asc_rpc::server::ServerInfo,
) -> tonic::service::Routes {
    let routes = crate::build_routes(ext, admission.recording(authorizer), info);
    tonic::service::Routes::from(
        routes
            .into_axum_router()
            .layer(AdmissionLayer::new(admission)),
    )
}

/// Installs the real [`TextBudget`] (`limits.text_memory`) on `ext`.
pub fn register(ext: &crate::Extensions, limits: &Limits) {
    let budget = Arc::new(TextMemory::new(limits.text_memory.0));
    if ext.text_budget.set(budget).is_err() {
        tracing::warn!("a text budget was already registered; keeping it");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use asc_rpc::retry::retry_after;
    use asc_rpc::server::status_rpc_error;
    use tonic::Code;

    use super::*;
    use crate::config::ByteSize;

    /// A verified principal.
    fn caller(n: u8) -> Caller {
        Caller::principal(&format!("user-{n}"))
    }

    fn limits(f: impl FnOnce(&mut Limits)) -> Limits {
        let mut l = Limits::default();
        f(&mut l);
        l
    }

    fn is_rate_limited(s: &Status) -> bool {
        s.code() == Code::ResourceExhausted
            && matches!(status_rpc_error(s), Some(RpcError::RateLimited(_)))
    }

    #[test]
    fn classes_follow_the_spec_table() {
        let c = |m: &str| class_of(&format!("/asc.v1.Repo/{m}"));
        assert_eq!(c("Version"), Some(Class::Cheap));
        assert_eq!(c("GetObjects"), Some(Class::Objects));
        assert_eq!(c("PutObjects"), Some(Class::Objects));
        assert_eq!(c("UpdateRefs"), Some(Class::UpdateRefs));
        assert_eq!(c("Blame"), Some(Class::Expensive));
        assert_eq!(c("Ingest"), Some(Class::Background));
        assert_eq!(c("SomethingNew"), Some(Class::Expensive));
        assert_eq!(class_of("/asc.v1.CodeIntel/Semantic"), Some(Class::Semantic));
        assert_eq!(class_of("/asc.v1.CodeIntel/Search"), Some(Class::Codeintel));
        assert_eq!(class_of("/asc.v1.CodeIntel/IndexStatus"), Some(Class::Cheap));
        assert_eq!(
            class_of("/asc.v1.Cloud/UpdateWorkspace"),
            Some(Class::Background)
        );
        assert_eq!(class_of("/asc.v1.Cloud/GetWorkspace"), Some(Class::Cheap));
        assert_eq!(class_of("/asc.v1.Changes/InterDiff"), Some(Class::Expensive));
        assert_eq!(class_of("/asc.v1.Changes/Stack"), Some(Class::Cheap));
        assert_eq!(class_of("/grpc.health.v1.Health/Check"), None);
        assert_eq!(class_of("/metrics"), None);
    }

    #[test]
    fn a_burst_above_the_limit_gets_rate_limited_with_retry_after() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 5;
            l.cheap_rps = 10;
        }));
        let who = caller(1);
        let held: Vec<_> = (0..5)
            .map(|_| a.admit(who, Class::Cheap, 1).unwrap())
            .collect();
        let err = a.admit(who, Class::Cheap, 1).err().unwrap();
        assert!(is_rate_limited(&err));
        let after = retry_after(&err).unwrap();
        assert!(after >= Duration::from_millis(1) && after <= Duration::from_millis(150));
        // Another principal is unaffected.
        a.admit(caller(2), Class::Cheap, 1).unwrap();
        drop(held);
    }

    #[test]
    fn in_flight_overflow_is_shed_not_queued() {
        let a = Admission::new(&limits(|l| l.max_inflight = 2));
        let started = Instant::now();
        let p1 = a.admit(caller(1), Class::Cheap, 1).unwrap();
        let _p2 = a.admit(caller(2), Class::Cheap, 1).unwrap();
        let err = a.admit(caller(3), Class::Cheap, 1).err().unwrap();
        assert_eq!(err.code(), Code::Unavailable);
        assert!(retry_after(&err).is_some());
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(a.in_flight(), 2);
        drop(p1);
        assert_eq!(a.in_flight(), 1);
        a.admit(caller(3), Class::Cheap, 1).unwrap();
    }

    #[test]
    fn one_token_cannot_exceed_its_in_flight_cap() {
        let a = Admission::new(&limits(|l| l.max_inflight_per_token = 3));
        let who = caller(1);
        let held: Vec<_> = (0..3)
            .map(|_| a.admit(who, Class::Cheap, 1).unwrap())
            .collect();
        assert!(is_rate_limited(
            &a.admit(who, Class::Cheap, 1).err().unwrap()
        ));
        a.admit(caller(2), Class::Cheap, 1).unwrap();
        drop(held);
        a.admit(who, Class::Cheap, 1).unwrap();
    }

    #[test]
    fn expensive_calls_are_capped_per_principal_and_per_replica() {
        let a = Admission::new(&limits(|l| {
            l.expensive_per_principal = 2;
            l.expensive_per_replica = 3;
        }));
        let (x, y) = (caller(1), caller(2));
        let _a1 = a.admit(x, Class::Expensive, 1).unwrap();
        let _a2 = a.admit(x, Class::Expensive, 1).unwrap();
        assert!(is_rate_limited(
            &a.admit(x, Class::Expensive, 1).err().unwrap()
        ));
        let _b1 = a.admit(y, Class::Expensive, 1).unwrap();
        // Three are running on the replica, so a fourth principal is shed.
        let err = a.admit(caller(3), Class::Expensive, 1).err().unwrap();
        assert!(retry_after(&err).is_some());
        // A shed request leaves nothing held.
        assert_eq!(a.in_flight(), 3);
    }

    #[test]
    fn objects_are_limited_by_bytes_per_second() {
        let a = Admission::new(&limits(|l| l.objects_bytes_per_sec = ByteSize(1000)));
        let who = caller(1);
        let _p = a.admit(who, Class::Objects, 600).unwrap();
        assert!(is_rate_limited(
            &a.admit(who, Class::Objects, 600).err().unwrap()
        ));
        // A request bigger than a second's budget is capped, not refused forever.
        let b = Admission::new(&limits(|l| l.objects_bytes_per_sec = ByteSize(1000)));
        b.admit(who, Class::Objects, 1 << 30).unwrap();
    }

    #[test]
    fn update_refs_and_semantic_have_their_own_limits() {
        let a = Admission::new(&limits(|l| {
            l.update_refs_rps_per_repo = 2;
            l.semantic_rps = 100;
            l.semantic_concurrency = 1;
        }));
        let who = caller(1);
        a.admit(who, Class::UpdateRefs, 1).unwrap();
        a.admit(who, Class::UpdateRefs, 1).unwrap();
        assert!(is_rate_limited(
            &a.admit(who, Class::UpdateRefs, 1).err().unwrap()
        ));
        let _s = a.admit(who, Class::Semantic, 1).unwrap();
        assert!(a.admit(caller(2), Class::Semantic, 1).is_err());
    }

    #[test]
    fn background_is_shed_before_interactive_work() {
        let a = Admission::new(&limits(|l| l.max_inflight = 8));
        let held: Vec<_> = (0..7u8)
            .map(|n| a.admit(caller(n), Class::Cheap, 1).unwrap())
            .collect();
        // One slot is free, below a quarter of the cap: background is shed, cheap still fits.
        let err = a.admit(caller(100), Class::Background, 1).err().unwrap();
        assert_eq!(err.code(), Code::Unavailable);
        a.admit(caller(101), Class::Cheap, 1).unwrap();
        drop(held);
    }

    #[test]
    fn the_service_class_has_its_own_budget() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 1;
            l.service_burst = 50;
        }))
        .with_service_token("forgejo");
        let svc = a.identify(Some("forgejo"), None);
        assert_eq!(svc, Caller::Service);
        for _ in 0..10 {
            a.admit(svc, Class::Cheap, 1).unwrap();
        }
        a.admit(caller(1), Class::Cheap, 1).unwrap();
        assert!(a.admit(caller(1), Class::Cheap, 1).is_err());
    }

    #[tokio::test]
    async fn ten_diffs_of_400_mib_shed_instead_of_exceeding_a_1_gib_budget() {
        const MIB: u64 = 1 << 20;
        let budget = Arc::new(TextMemory::with_wait(
            1024 * MIB,
            Duration::from_millis(100),
        ));
        let held = Arc::new(AtomicU64::new(0));
        let peak = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (budget, held, peak) = (budget.clone(), held.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                let guard = budget.acquire(400 * MIB).await.ok()?;
                let now = held.fetch_add(400 * MIB, Ordering::SeqCst) + 400 * MIB;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(400)).await;
                held.fetch_sub(400 * MIB, Ordering::SeqCst);
                drop(guard);
                Some(())
            }));
        }
        let mut served = 0;
        for t in tasks {
            served += usize::from(t.await.unwrap().is_some());
        }
        assert_eq!(served, 2, "only two 400 MiB buffers fit in 1 GiB");
        assert!(peak.load(Ordering::SeqCst) <= 1024 * MIB);
        assert_eq!(budget.available(), 1024 * MIB);
    }

    #[tokio::test]
    async fn text_memory_waits_for_release_then_grants() {
        let budget = Arc::new(TextMemory::with_wait(
            10 * TEXT_UNIT,
            Duration::from_secs(5),
        ));
        let first = budget.acquire(8 * TEXT_UNIT).await.unwrap();
        let waiter = {
            let b = budget.clone();
            tokio::spawn(async move { b.acquire(8 * TEXT_UNIT).await.is_ok() })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(first);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn text_memory_refuses_what_can_never_fit() {
        let budget = TextMemory::with_wait(10 * TEXT_UNIT, Duration::from_secs(5));
        let started = Instant::now();
        assert!(budget.acquire(11 * TEXT_UNIT).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        let status = Status::from(TextBudgetExhausted);
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn register_installs_the_real_budget() {
        let ext = crate::Extensions::new();
        register(&ext, &limits(|l| l.text_memory = ByteSize(4 * TEXT_UNIT)));
        let _g = ext.text_budget.acquire(3 * TEXT_UNIT).await.unwrap();
        assert!(ext.text_budget.acquire(10 * TEXT_UNIT).await.is_err());
    }

    #[derive(Clone)]
    struct Echo;

    impl Service<Request<Body>> for Echo {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Response<Body>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<Body>) -> Self::Future {
            std::future::ready(Ok(Response::new(Body::from("ok"))))
        }
    }

    fn peer(n: u8) -> TcpConnectInfo {
        TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(std::net::SocketAddr::from(([10, 0, 0, n], 40_000))),
        }
    }

    fn request(path: &str, token: &str, from: u8, body: Body) -> Request<Body> {
        let mut req = Request::builder()
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .body(body)
            .unwrap();
        req.extensions_mut().insert(peer(from));
        req
    }

    fn call<S>(svc: &mut AdmissionService<S>, path: &str, token: &str) -> Response<Body>
    where
        S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        call_from(svc, path, token, 1)
    }

    fn call_from<S>(
        svc: &mut AdmissionService<S>,
        path: &str,
        token: &str,
        from: u8,
    ) -> Response<Body>
    where
        S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        let req = request(path, token, from, Body::empty());
        futures::executor::block_on(svc.call(req)).unwrap()
    }

    fn shed(resp: &Response<Body>) -> bool {
        resp.headers().contains_key("grpc-status")
    }

    #[test]
    fn the_layer_sheds_with_grpc_status_and_holds_the_slot_until_the_body_drops() {
        let admission = Arc::new(Admission::new(&limits(|l| l.max_inflight = 1)));
        let mut svc = AdmissionLayer::new(admission.clone()).layer(Echo);
        let ok = call(&mut svc, "/asc.v1.Repo/Version", "t");
        assert_eq!(admission.in_flight(), 1);
        let shed = call(&mut svc, "/asc.v1.Repo/Version", "t");
        let headers = shed.headers();
        assert_eq!(
            headers.get("grpc-status").unwrap(),
            &(Code::Unavailable as i32).to_string()
        );
        assert!(headers.contains_key(RETRY_AFTER_MS));
        // Routes outside the asc.v1 services are not limited.
        let other = call(&mut svc, "/metrics", "t");
        assert_eq!(other.status(), 200);
        drop(ok);
        assert_eq!(admission.in_flight(), 0);
        drop(call(&mut svc, "/asc.v1.Repo/Version", "t"));
    }

    // Finding 1: admission runs before authentication, so a token proves nothing.
    #[test]
    fn fresh_random_tokens_from_one_peer_share_one_bucket() {
        let admission = Arc::new(Admission::new(&limits(|l| {
            l.cheap_burst = 5;
            l.cheap_rps = 1;
        })));
        let mut svc = AdmissionLayer::new(admission).layer(Echo);
        let admitted = (0..200)
            .filter(|n| {
                let token = format!("rand-{n}");
                !shed(&call_from(&mut svc, "/asc.v1.Repo/Version", &token, 1))
            })
            .count();
        assert!(
            admitted <= 6,
            "{admitted} of 200 requests with distinct tokens got through"
        );
        // Another peer is a different unverified caller.
        assert!(!shed(&call_from(
            &mut svc,
            "/asc.v1.Repo/Version",
            "rand-x",
            2
        )));
        // The same peer with another token is still the same caller.
        assert!(shed(&call_from(
            &mut svc,
            "/asc.v1.Repo/Version",
            "other",
            1
        )));
    }

    struct Accepts;

    #[async_trait]
    impl Authorizer for Accepts {
        async fn authorize(
            &self,
            token: Option<&BearerToken>,
            _repo: &str,
            _access: Access,
        ) -> Result<Principal, AuthError> {
            match token.map(BearerToken::expose) {
                Some("alice-token" | "alice-other-token") => Ok(Principal { id: "alice".into() }),
                Some("bob-token") => Ok(Principal { id: "bob".into() }),
                _ => Err(AuthError::Forbidden),
            }
        }
    }

    #[tokio::test]
    async fn a_token_the_authorizer_accepted_gets_its_own_bucket_per_principal() {
        let admission = Arc::new(Admission::new(&limits(|l| {
            l.cheap_burst = 3;
            l.cheap_rps = 1;
        })));
        let mut svc = AdmissionLayer::new(admission.clone()).layer(Echo);
        let authz = admission.recording(Arc::new(Accepts));
        // The peer's unverified bucket is spent.
        for _ in 0..10 {
            call_from(&mut svc, "/asc.v1.Repo/Version", "alice-token", 1);
        }
        assert!(shed(&call_from(
            &mut svc,
            "/asc.v1.Repo/Version",
            "alice-token",
            1
        )));
        // A refused token is not recorded, so it gains nothing.
        let bad = BearerToken::new("alice-token-but-wrong").unwrap();
        assert!(
            authz
                .authorize(Some(&bad), "o/r", Access::Read)
                .await
                .is_err()
        );
        let wrong = call_from(&mut svc, "/asc.v1.Repo/Version", "alice-token-but-wrong", 1);
        assert!(shed(&wrong));
        // Once the authorizer accepts the token, it is charged as alice, from any peer.
        let good = BearerToken::new("alice-token").unwrap();
        authz
            .authorize(Some(&good), "o/r", Access::Read)
            .await
            .unwrap();
        for _ in 0..3 {
            assert!(!shed(&call_from(
                &mut svc,
                "/asc.v1.Repo/Version",
                "alice-token",
                1
            )));
        }
        assert!(shed(&call_from(
            &mut svc,
            "/asc.v1.Repo/Version",
            "alice-token",
            1
        )));
        // Another token of the same principal shares alice's bucket, so tokens do not multiply it.
        let other = BearerToken::new("alice-other-token").unwrap();
        authz
            .authorize(Some(&other), "o/r", Access::Read)
            .await
            .unwrap();
        let again = call_from(&mut svc, "/asc.v1.Repo/Version", "alice-other-token", 7);
        assert!(shed(&again));
        // Another principal is unaffected.
        let bob = BearerToken::new("bob-token").unwrap();
        authz
            .authorize(Some(&bob), "o/r", Access::Read)
            .await
            .unwrap();
        assert!(!shed(&call_from(
            &mut svc,
            "/asc.v1.Repo/Version",
            "bob-token",
            1
        )));
    }

    // Finding 3: a table full of unverified callers must not shed a newcomer.
    #[tokio::test]
    async fn a_full_table_still_admits_new_callers() {
        let a = Admission::with_table_size(
            &limits(|l| {
                l.cheap_burst = 5;
                l.cheap_rps = 1;
            }),
            4,
        );
        let authz = a.recording(Arc::new(Accepts));
        let ip = |n: u8| Some(IpAddr::from([10, 0, 0, n]));
        // Four peers each spend some of their bucket, so none is idle and none can be pruned.
        for n in 1..=4 {
            a.admit(a.identify(None, ip(n)), Class::Cheap, 1).unwrap();
        }
        // A fifth peer is not refused outright; it uses the shared overflow bucket (burst 5).
        a.admit(a.identify(None, ip(5)), Class::Cheap, 1).unwrap();
        // A flood of peers cannot evict a tracked one: peer 1 still has its own bucket.
        for n in 6..=200 {
            let _ = a.admit(a.identify(None, ip(n)), Class::Cheap, 1);
        }
        a.admit(a.identify(None, ip(1)), Class::Cheap, 1).unwrap();
        // Verified callers have their own table, which the flood did not touch.
        for t in ["alice-token", "bob-token"] {
            let t = BearerToken::new(t).unwrap();
            authz
                .authorize(Some(&t), "o/r", Access::Read)
                .await
                .unwrap();
        }
        let alice = a.identify(Some("alice-token"), ip(250));
        assert!(matches!(alice, Caller::Verified(_)));
        a.admit(alice, Class::Cheap, 1).unwrap();
    }

    #[test]
    fn a_full_verified_tier_evicts_the_least_recent_principal_instead_of_shedding() {
        let a = Admission::with_table_size(&limits(|l| l.cheap_burst = 5), 3);
        for n in 1..=3 {
            a.admit(caller(n), Class::Cheap, 1).unwrap();
        }
        a.admit(caller(4), Class::Cheap, 1).unwrap();
    }

    #[test]
    fn peers_group_by_ipv6_prefix_and_mapped_ipv4() {
        let v6 = |s: &str| Peer::from_ip(Some(s.parse().unwrap()));
        assert_eq!(v6("2001:db8:1:2::1"), v6("2001:db8:1:2:ffff::9"));
        assert_ne!(v6("2001:db8:1:2::1"), v6("2001:db8:1:3::1"));
        let v4 = Peer::from_ip(Some("10.0.0.1".parse().unwrap()));
        assert_eq!(v6("::ffff:10.0.0.1"), v4);
        assert_eq!(Peer::from_ip(None), Peer::Unknown);
    }

    // The server wires admission through `guarded_routes`; this fails if that stops applying it.
    #[tokio::test]
    async fn the_served_routes_apply_admission() {
        let ext = crate::Extensions::new();
        let admission = Arc::new(Admission::new(&limits(|l| {
            l.cheap_burst = 2;
            l.cheap_rps = 1;
        })));
        let routes = guarded_routes(
            &ext,
            Arc::new(crate::DenyAll),
            admission,
            asc_rpc::server::ServerInfo::default(),
        );
        let router = routes.into_axum_router();
        let mut refused = 0;
        for _ in 0..6 {
            let mut req = request("/asc.v1.Repo/Version", "tok", 1, Body::empty());
            req.headers_mut()
                .insert("content-type", "application/grpc".parse().unwrap());
            let resp = router.clone().call(req).await.unwrap();
            let status = resp.headers().get("grpc-status");
            if status.is_some_and(|s| *s == (Code::ResourceExhausted as i32).to_string()) {
                assert!(resp.headers().contains_key(RETRY_AFTER_MS));
                refused += 1;
            }
        }
        assert_eq!(refused, 4, "all but the burst of 2 are refused");
    }

    /// Reads a request body to its end or its first error, and reports what it saw in headers.
    #[derive(Clone)]
    struct Drain;

    impl Service<Request<Body>> for Drain {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = BoxFuture<'static, Result<Response<Body>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            Box::pin(async move {
                let mut body = req.into_body();
                let (mut bytes, mut failed) = (0usize, false);
                while let Some(frame) =
                    std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
                {
                    match frame {
                        Ok(f) => bytes += f.data_ref().map_or(0, Bytes::len),
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
                let resp = Response::builder()
                    .header("x-bytes", bytes.to_string())
                    .header("x-failed", failed.to_string())
                    .body(Body::empty())
                    .unwrap();
                Ok(resp)
            })
        }
    }

    fn chunks(n: usize, size: usize) -> Body {
        let frames = (0..n).map(move |_| Ok::<_, Infallible>(Bytes::from(vec![0u8; size])));
        Body::from_stream(futures::stream::iter(frames))
    }

    fn put(svc: &mut AdmissionService<Drain>, length: Option<&str>, body: Body) -> Response<Body> {
        let mut req = request("/asc.v1.Repo/PutObjects", "t", 1, body);
        if let Some(l) = length {
            req.headers_mut()
                .insert("content-length", l.parse().unwrap());
        }
        futures::executor::block_on(svc.call(req)).unwrap()
    }

    fn header<'a>(r: &'a Response<Body>, name: &str) -> &'a str {
        r.headers().get(name).unwrap().to_str().unwrap()
    }

    fn byte_budget(bytes: u64) -> Arc<Admission> {
        Arc::new(Admission::new(&limits(|l| {
            l.objects_bytes_per_sec = ByteSize(bytes);
        })))
    }

    // Finding 2: the cost of a body is what it really holds, not what its header says.
    #[test]
    fn a_body_is_charged_its_real_size_whatever_content_length_says() {
        let mut svc = AdmissionLayer::new(byte_budget(1000)).layer(Drain);
        // 10 000 bytes in 500-byte frames, under a header that claims 1 byte.
        let lied = put(&mut svc, Some("1"), chunks(20, 500));
        assert_eq!(header(&lied, "x-failed"), "true");
        assert!(header(&lied, "x-bytes").parse::<usize>().unwrap() <= 1500);
    }

    #[test]
    fn a_chunked_body_with_no_length_is_charged_its_real_size() {
        let mut svc = AdmissionLayer::new(byte_budget(1000)).layer(Drain);
        let big = put(&mut svc, None, chunks(20, 500));
        assert_eq!(header(&big, "x-failed"), "true");
        // A body within the budget goes through whole, and the next one finds it half spent.
        let mut svc = AdmissionLayer::new(byte_budget(4000)).layer(Drain);
        let small = put(&mut svc, None, chunks(4, 500));
        assert_eq!(header(&small, "x-failed"), "false");
        assert_eq!(header(&small, "x-bytes"), "2000");
        let next = put(&mut svc, None, chunks(6, 500));
        assert_eq!(header(&next, "x-failed"), "true");
    }

    /// Answers every call with a body of `n` bytes in 500-byte frames.
    #[derive(Clone)]
    struct Download(usize);

    impl Service<Request<Body>> for Download {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Response<Body>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<Body>) -> Self::Future {
            std::future::ready(Ok(Response::new(chunks(self.0 / 500, 500))))
        }
    }

    #[test]
    fn response_bytes_of_an_objects_call_are_charged_too() {
        let mut svc = AdmissionLayer::new(byte_budget(1000)).layer(Download(10_000));
        let mut resp = call(&mut svc, "/asc.v1.Repo/GetObjects", "t");
        assert!(!shed(&resp));
        // The stream is not cut; it is read in full, and the caller owes the bytes.
        let mut sent = 0;
        futures::executor::block_on(async {
            while let Some(f) =
                std::future::poll_fn(|cx| Pin::new(resp.body_mut()).poll_frame(cx)).await
            {
                sent += f.unwrap().data_ref().map_or(0, Bytes::len);
            }
        });
        assert_eq!(sent, 10_000);
        assert!(shed(&call(&mut svc, "/asc.v1.Repo/GetObjects", "t")));
        // A cheap call is a different bucket.
        assert!(!shed(&call(&mut svc, "/asc.v1.Repo/Version", "t")));
    }
}
