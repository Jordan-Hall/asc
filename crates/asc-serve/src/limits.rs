//! Admission control for `asc serve` (SPEC §16.3, §16.4): per-principal rate classes, in-flight
//! caps, load shedding, and the `limits.text_memory` byte semaphore.
//!
//! Nothing here queues. A request over a limit is refused at once with a `retry-after-ms`
//! trailer; only [`TextMemory`] waits, and for at most one second.

use std::collections::HashSet;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, Bytes, HttpBody};
use axum::http::{Request, Response};
use asc_rpc::errors::RpcError;
use asc_rpc::limits::{CounterGuard, KeyedCounter, KeyedLimiter, Shed};
use asc_rpc::retry::RETRY_AFTER_MS;
use asc_rpc::server::rpc_status;
use futures::future::BoxFuture;
use http_body::Frame;
use tokio::sync::Semaphore;
use tonic::Status;
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

/// Who is calling, as far as admission can tell before authorisation: a hash of the bearer
/// token (SPEC §16.3 keys by the authz user id or the token hash).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Caller([u8; 32]);

impl Caller {
    pub fn from_token(token: Option<&str>) -> Self {
        Self(*blake3::hash(token.unwrap_or_default().as_bytes()).as_bytes())
    }

    fn from_headers(headers: &axum::http::HeaderMap) -> Self {
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        Self::from_token(token)
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
    cheap: KeyedLimiter<Caller>,
    objects: KeyedLimiter<Caller>,
    update_refs: KeyedLimiter<Caller>,
    codeintel: KeyedLimiter<Caller>,
    semantic: KeyedLimiter<Caller>,
    service: KeyedLimiter<Caller>,
    expensive_principal: Arc<KeyedCounter<Caller>>,
    expensive_replica: Arc<Semaphore>,
    semantic_replica: Arc<Semaphore>,
    service_callers: HashSet<Caller>,
}

impl Admission {
    pub fn new(cfg: &Limits) -> Self {
        let rate = |n: u32| u64::from(n);
        Self {
            cfg: cfg.clone(),
            inflight: Arc::new(Semaphore::new(cfg.max_inflight as usize)),
            per_token: Arc::new(KeyedCounter::new()),
            cheap: KeyedLimiter::new(rate(cfg.cheap_rps), rate(cfg.cheap_burst)),
            objects: KeyedLimiter::new(cfg.objects_bytes_per_sec.0, cfg.objects_bytes_per_sec.0),
            // The burst is one second of the per-repo rate; the key is the caller (see `admit`).
            update_refs: KeyedLimiter::new(
                rate(cfg.update_refs_rps_per_repo),
                rate(cfg.update_refs_rps_per_repo),
            ),
            codeintel: KeyedLimiter::new(rate(cfg.codeintel_rps), rate(cfg.codeintel_burst)),
            semantic: KeyedLimiter::new(rate(cfg.semantic_rps), rate(cfg.semantic_rps)),
            service: KeyedLimiter::new(rate(cfg.service_rps), rate(cfg.service_burst)),
            expensive_principal: Arc::new(KeyedCounter::new()),
            expensive_replica: Arc::new(Semaphore::new(cfg.expensive_per_replica as usize)),
            semantic_replica: Arc::new(Semaphore::new(cfg.semantic_concurrency as usize)),
            service_callers: HashSet::new(),
        }
    }

    /// Treats calls bearing `token` as the `service` class (Forgejo, the CI warmer).
    pub fn with_service_token(mut self, token: &str) -> Self {
        self.service_callers.insert(Caller::from_token(Some(token)));
        self
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

        let class = if self.service_callers.contains(&who) {
            Class::Service
        } else {
            class
        };
        match class {
            Class::Cheap | Class::Background => self.cheap.check(&who, 1),
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

/// A response body that keeps the request's [`Permit`] until the stream ends or is dropped.
struct Held {
    inner: Body,
    _permit: Permit,
}

impl HttpBody for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
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
        let who = Caller::from_headers(req.headers());
        let cost = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        match self.admission.admit(who, class, cost) {
            Err(status) => Box::pin(std::future::ready(Ok(status.into_http()))),
            Ok(permit) => Box::pin(async move {
                let resp = inner.call(req).await?;
                Ok(resp.map(|body| {
                    Body::new(Held {
                        inner: body,
                        _permit: permit,
                    })
                }))
            }),
        }
    }
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

    fn caller(n: u8) -> Caller {
        Caller::from_token(Some(&format!("token-{n}")))
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
        assert_eq!(
            class_of("/asc.v1.CodeIntel/Semantic"),
            Some(Class::Semantic)
        );
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
        assert!(is_rate_limited(&a.admit(who, Class::Cheap, 1).err().unwrap()));
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
        assert!(is_rate_limited(&a.admit(x, Class::Expensive, 1).err().unwrap()));
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
        assert!(is_rate_limited(&a.admit(who, Class::Objects, 600).err().unwrap()));
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
        assert!(is_rate_limited(&a.admit(who, Class::UpdateRefs, 1).err().unwrap()));
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
        let svc = Caller::from_token(Some("forgejo"));
        for _ in 0..10 {
            a.admit(svc, Class::Cheap, 1).unwrap();
        }
        a.admit(caller(1), Class::Cheap, 1).unwrap();
        assert!(a.admit(caller(1), Class::Cheap, 1).is_err());
    }

    #[tokio::test]
    async fn ten_diffs_of_400_mib_shed_instead_of_exceeding_a_1_gib_budget() {
        const MIB: u64 = 1 << 20;
        let budget = Arc::new(TextMemory::with_wait(1024 * MIB, Duration::from_millis(100)));
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
        let budget = Arc::new(TextMemory::with_wait(10 * TEXT_UNIT, Duration::from_secs(5)));
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

    fn call(svc: &mut AdmissionService<Echo>, path: &str, token: &str) -> Response<Body> {
        let req = Request::builder()
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        futures::executor::block_on(svc.call(req)).unwrap()
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
}
