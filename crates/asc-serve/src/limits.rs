//! Admission control for `asc serve` (SPEC §16.3, §16.4): per-principal rate classes, in-flight
//! caps, load shedding, and the `limits.text_memory` byte semaphore.
//!
//! Nothing here queues. A request over a limit is refused at once with a `retry-after-ms`
//! trailer; only [`TextMemory`] waits, and for at most one second.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Credential, Principal};
use asc_rpc::errors::RpcError;
use asc_rpc::limits::{CounterGuard, KeyedCounter, KeyedLimiter, MAX_KEYS, OnFull, Shed};
use asc_rpc::retry::RETRY_AFTER_MS;
use asc_rpc::server::rpc_status;
use async_trait::async_trait;
use axum::body::{Body, Bytes, HttpBody};
use axum::http::{Request, Response};
use futures::future::BoxFuture;
use http_body::Frame;
use tokio::sync::Semaphore;
use tonic::Status;
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
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
    /// A `git-upload-pack` fetch over smart-HTTP (SPEC §25.5): holds one of `limits.git`'s
    /// per-principal and per-replica fetch slots for its whole run, and its bytes in and out are
    /// charged to the `objects` bucket as they stream. Discovery (`info/refs`) is `Cheap`.
    Git,
}

/// The class of a git smart-HTTP request (`/<owner>/<repo>/info/refs`, `…/git-upload-pack`,
/// `…/git-receive-pack`), or `None` for any other path. The path is read as the client sent it;
/// the handler validates the name (SPEC §25.8 item 1), so a hostile path costs a `Cheap` unit
/// here and is a `404` there.
pub fn git_class_of(path: &str) -> Option<Class> {
    let mut parts = path.strip_prefix('/')?.split('/');
    let (Some(_owner), Some(_repo), Some(first)) = (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    match (first, parts.next(), parts.next()) {
        ("info", Some("refs"), None) => Some(Class::Cheap),
        ("git-upload-pack", None, None) => Some(Class::Git),
        // Pushes get their own caps with the receive-pack path (U82).
        ("git-receive-pack", None, None) => Some(Class::Cheap),
        _ => None,
    }
}

/// A refusal of admission as the plain HTTP answer a git client understands: `429` or `503` with
/// `Retry-After` in whole seconds (git does not retry; it prints the status).
fn git_refusal(status: &Status) -> Response<Body> {
    let code = match status.code() {
        tonic::Code::ResourceExhausted => axum::http::StatusCode::TOO_MANY_REQUESTS,
        tonic::Code::Unavailable => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    };
    let ms: u64 = status
        .metadata()
        .get(RETRY_AFTER_MS)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let mut res = Response::new(Body::from(
        "asc: too many git requests; retry later
",
    ));
    *res.status_mut() = code;
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from(ms.div_ceil(1000).max(1)),
    );
    res
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
        ("Inbox", "List" | "Count") => Class::Cheap,
        ("Review", "ListThreads") => Class::Cheap,
        ("Ci", "ListRuns" | "JobLog") => Class::Cheap,
        ("Inbox" | "Review" | "Ci", _) => Class::Background,
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
/// The longest a token stays verified since the authorizer last accepted it, however often it
/// is used in between. Use renews the lifetime (see [`Verified::touch`]) but never past this, so
/// a token that has been revoked cannot hold a bucket of its own for ever.
const VERIFIED_MAX_AGE: Duration = Duration::from_secs(600);
/// Hops of `X-Forwarded-For` examined, counted from the right, and bytes of each header value.
const XFF_MAX_HOPS: usize = 32;
const XFF_MAX_BYTES: usize = 2048;
/// Verified tokens remembered at once. Past this, a newly verified token stays in the peer
/// tier until an entry expires.
const MAX_VERIFIED: usize = 100_000;
/// Requests a not-yet-verified token may make past a spent peer bucket, so that a legitimate
/// token behind a saturated NAT or proxy can still reach the authorizer and become verified.
const BOOTSTRAP_ALLOWANCE: u32 = 3;
/// How long a token's bootstrap allowance lasts before it is reset.
const BOOTSTRAP_TTL: Duration = Duration::from_secs(10);
/// Tokens in their bootstrap window at once. Past this, further tokens get none.
const MAX_BOOTSTRAP: usize = 1024;
/// New tokens per second, and in one burst, that one peer may start a bootstrap window for.
const BOOTSTRAP_PEER_RATE: u64 = 2;
const BOOTSTRAP_PEER_BURST: u64 = 8;
/// A request without a peer address is logged at most this often.
const UNKNOWN_PEER_WARN_EVERY: Duration = Duration::from_secs(60);

/// An address range, for `limits.trusted_proxies`. A bare address is a single-host range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    net: u128,
    mask: u128,
}

fn ip_bits(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(a) => u128::from(a.to_ipv6_mapped()),
        IpAddr::V6(a) => u128::from(a),
    }
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let ip: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("bad address `{addr}`"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("bad prefix `{p}`"))?,
            None => max,
        };
        if prefix > max {
            return Err(format!("prefix /{prefix} is too long"));
        }
        // IPv4 lives in the IPv4-mapped IPv6 range, so one mask covers both families.
        let len = if ip.is_ipv4() { 96 + prefix } else { prefix };
        let mask = if len == 0 {
            0
        } else {
            u128::MAX << (128 - len)
        };
        Ok(Self {
            net: ip_bits(ip) & mask,
            mask,
        })
    }
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        ip_bits(ip) & self.mask == self.net
    }
}

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
    /// A token that has not been verified yet and is spending its bootstrap allowance (see
    /// [`Bootstrap`]). It has no rate bucket: the allowance is its limit. Its in-flight slots are
    /// its peer's.
    Bootstrap { token: [u8; 32], peer: Peer },
}

impl Caller {
    pub(crate) fn principal(id: &str) -> Self {
        Self::Verified(*blake3::hash(id.as_bytes()).as_bytes())
    }
}

fn token_hash(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

/// The TCP peer of a request. tonic puts `TcpConnectInfo` on every request and, over TLS,
/// `TlsConnectInfo` too; this reads both, as `tonic::Request::remote_addr` does.
pub(crate) fn connect_addr(ext: &axum::http::Extensions) -> Option<IpAddr> {
    let addr = ext
        .get::<TcpConnectInfo>()
        .and_then(TcpConnectInfo::remote_addr)
        .or_else(|| {
            ext.get::<TlsConnectInfo<TcpConnectInfo>>()
                .and_then(|i| i.get_ref().remote_addr())
        });
    addr.map(|a| a.ip())
}

tokio::task_local! {
    /// The network peer of the request being served, set by [`AdmissionLayer`] around the whole
    /// call so a handler (the `WatchRefs` stream limit) can charge an anonymous caller to its
    /// address without every `CallCtx` carrying one.
    static CLIENT_PEER: Peer;
}

/// The peer of the request being served, or [`Peer::Unknown`] outside a request.
pub(crate) fn current_peer() -> Peer {
    CLIENT_PEER.try_with(|p| *p).unwrap_or(Peer::Unknown)
}

/// Runs `fut` with `peer` as [`current_peer`].
pub(crate) fn with_peer<F: Future>(peer: Peer, fut: F) -> impl Future<Output = F::Output> {
    CLIENT_PEER.scope(peer, fut)
}

/// The secret of a request's credential, whatever the scheme (`Basic`, `Bearer` or `token`), by
/// the parser the interceptor and every route share (U78). Admission runs before authentication
/// and never refuses on a bad header, so one the parser rejects is anonymous here; the handler
/// then answers it. The hash [`token_hash`] takes this secret, never the header, so the three
/// schemes carrying one token are one caller.
fn credential_secret(headers: &axum::http::HeaderMap) -> Option<String> {
    crate::authn::credential(headers)
        .ok()
        .flatten()
        .map(|c| c.secret().expose().to_owned())
}

/// The tokens the authorizer has accepted recently, and the principal each belongs to.
///
/// Only [`RecordingAuthorizer`] writes it, and only after a real authorisation succeeded, so an
/// attacker cannot add entries with made-up tokens. It is bounded: when full of live entries,
/// new tokens are simply not recorded and stay in the peer tier.
pub struct Verified {
    ttl: Duration,
    max_age: Duration,
    max: usize,
    inner: Mutex<VerifiedMap>,
}

/// One accepted token.
struct VerifiedEntry {
    /// When the authorizer last accepted the token. The entry never outlives `max_age` from here.
    accepted: Instant,
    /// The last accepted use; the entry lapses `ttl` after it.
    used: Instant,
    who: Caller,
}

#[derive(Default)]
struct VerifiedMap {
    tokens: HashMap<[u8; 32], VerifiedEntry>,
    last_prune: Option<Instant>,
}

impl Verified {
    fn new(ttl: Duration, max_age: Duration, max: usize) -> Self {
        Self {
            ttl,
            max_age,
            max,
            inner: Mutex::default(),
        }
    }

    fn live(&self, e: &VerifiedEntry, now: Instant) -> bool {
        Self::is_live(e, now, self.ttl, self.max_age)
    }

    fn is_live(e: &VerifiedEntry, now: Instant, ttl: Duration, max_age: Duration) -> bool {
        now.saturating_duration_since(e.used) < ttl
            && now.saturating_duration_since(e.accepted) < max_age
    }

    /// The authorizer accepted `token` for `principal`.
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
                let (ttl, max_age) = (self.ttl, self.max_age);
                g.tokens.retain(|_, e| Self::is_live(e, now, ttl, max_age));
            }
            if g.tokens.len() >= self.max {
                return;
            }
        }
        let who = Caller::principal(principal);
        g.tokens.insert(
            key,
            VerifiedEntry {
                accepted: now,
                used: now,
                who,
            },
        );
    }

    /// The authorizer refused `token`: it no longer has a bucket of its own.
    fn forget(&self, token_hash: &[u8; 32]) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.tokens.remove(token_hash);
    }

    /// Extends a live entry's lifetime from now. Called for every request admitted under the
    /// token, so a token in steady use stays verified however long the peer bucket stays
    /// saturated. It never extends past `max_age` since the authorizer last accepted the token.
    fn touch(&self, token_hash: &[u8; 32]) {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let live = g.tokens.get(token_hash).is_some_and(|e| self.live(e, now));
        if live && let Some(e) = g.tokens.get_mut(token_hash) {
            e.used = now;
        }
    }

    fn lookup(&self, token_hash: &[u8; 32]) -> Option<Caller> {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let e = g.tokens.get(token_hash)?;
        self.live(e, Instant::now()).then_some(e.who)
    }
}

/// A small, bounded allowance of requests for tokens that are not verified yet. It applies to
/// cheap calls only, and only when the peer's rate bucket refused them.
///
/// Verification happens after admission, inside the authorizer. Behind a NAT or proxy a flood in
/// the peer bucket could keep a legitimate token from ever reaching it. So a token that the peer
/// bucket has refused gets [`BOOTSTRAP_ALLOWANCE`] requests per [`BOOTSTRAP_TTL`], charged to
/// no bucket. Each peer may start only a few windows per second ([`BOOTSTRAP_PEER_RATE`]), and
/// the table holds [`MAX_BOOTSTRAP`] tokens, so random tokens buy a small, bounded number of
/// allowances; past that, tokens are charged to the peer as before. A token the
/// authorizer refuses has its allowance spent at once.
struct Bootstrap {
    max: usize,
    inner: Mutex<BootstrapMap>,
}

#[derive(Default)]
struct BootstrapMap {
    /// Window start and requests left.
    entries: HashMap<[u8; 32], (Instant, u32)>,
    last_prune: Option<Instant>,
}

impl Bootstrap {
    fn new(max: usize) -> Self {
        Self {
            max,
            inner: Mutex::default(),
        }
    }

    /// Spends one request of the token's allowance, if it has any left.
    fn take(&self, token_hash: &[u8; 32]) -> bool {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((born, left)) = g.entries.get_mut(token_hash)
            && now.saturating_duration_since(*born) < BOOTSTRAP_TTL
        {
            if *left == 0 {
                return false;
            }
            *left -= 1;
            return true;
        }
        if g.entries.len() >= self.max {
            let due = g
                .last_prune
                .is_none_or(|p| now.saturating_duration_since(p) >= Duration::from_secs(1));
            if due {
                g.last_prune = Some(now);
                g.entries
                    .retain(|_, (born, _)| now.saturating_duration_since(*born) < BOOTSTRAP_TTL);
            }
            if g.entries.len() >= self.max && !g.entries.contains_key(token_hash) {
                return false;
            }
        }
        g.entries
            .insert(*token_hash, (now, BOOTSTRAP_ALLOWANCE - 1));
        true
    }

    /// The authorizer has decided: an accepted token no longer needs its allowance, and a
    /// refused one has none left.
    fn settle(&self, token_hash: &[u8; 32], accepted: bool) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if accepted {
            g.entries.remove(token_hash);
        } else if let Some(entry) = g.entries.get_mut(token_hash) {
            *entry = (Instant::now(), 0);
        } else if g.entries.len() < self.max {
            g.entries.insert(*token_hash, (Instant::now(), 0));
        }
    }
}

/// What admission knows about tokens, fed by [`RecordingAuthorizer`].
struct Identities {
    verified: Verified,
    bootstrap: Bootstrap,
}

/// An [`Authorizer`] that tells admission which tokens were accepted, so that those callers are
/// charged by principal and not by network peer.
pub struct RecordingAuthorizer {
    inner: Arc<dyn Authorizer>,
    ids: Arc<Identities>,
}

#[async_trait]
impl Authorizer for RecordingAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let credential = token.cloned().map(Credential::bearer);
        self.authorize_credential(credential.as_ref(), repo, access)
            .await
    }

    /// Forwards to the inner authorizer's own `authorize_credential` (which may use the Basic
    /// user name) and records the verdict under the hash of the credential's secret, the same
    /// hash admission computes from the header, so a verified token is one caller whichever of
    /// `Basic`, `Bearer` or `token` it arrives in.
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
        if let Some(c) = credential {
            let secret = c.secret().expose();
            match &answer {
                Ok(p) => self.ids.verified.record(secret, &p.id),
                // A refused token (revoked, or not allowed here) is no longer a known caller.
                Err(_) => self.ids.verified.forget(&token_hash(secret)),
            }
            self.ids
                .bootstrap
                .settle(&token_hash(secret), answer.is_ok());
        }
        answer
    }

    /// Forwards to the inner authorizer and records the verdict like a repository decision, so a
    /// token that authenticates is charged by principal.
    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        let answer = self.inner.authenticate(credential).await;
        if let Some(c) = credential {
            let secret = c.secret().expose();
            match &answer {
                Ok(p) => self.ids.verified.record(secret, &p.id),
                Err(_) => self.ids.verified.forget(&token_hash(secret)),
            }
            self.ids
                .bootstrap
                .settle(&token_hash(secret), answer.is_ok());
        }
        answer
    }

    fn forwarded_verifier(&self) -> Option<&asc_rpc::forwarded::ForwardedVerifier> {
        self.inner.forwarded_verifier()
    }

    /// SSH keys are the inner authorizer's to recognise; the listener's own admission limits
    /// them (U86), so this only forwards.
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
            Caller::Verified(_) | Caller::Service | Caller::Bootstrap { .. } => &self.verified,
        }
    }

    fn check(&self, who: &Caller, cost: u64) -> Result<(), Shed> {
        self.tier(who).check(who, cost)
    }

    /// Charges `cost` bytes in full, as debt if need be (see [`TokenBucket::take_overdraft`]).
    fn charge_at(&self, who: &Caller, cost: u64, now: Instant) -> Result<(), Shed> {
        self.tier(who).charge_at(who, cost, now)
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

/// Why admission said no.
struct Refusal {
    status: Status,
    /// The caller's own rate bucket was empty. Every other refusal (global overload, a
    /// concurrency cap, a replica cap) says nothing about this caller's rate and is final.
    bucket: bool,
}

impl Refusal {
    fn other(status: Status) -> Self {
        Self {
            status,
            bucket: false,
        }
    }
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
    git_principal: Arc<KeyedCounter<Caller>>,
    git_replica: Arc<Semaphore>,
    semantic_replica: Arc<Semaphore>,
    service_tokens: HashSet<[u8; 32]>,
    ids: Arc<Identities>,
    bootstrap_gate: KeyedLimiter<Caller>,
    trusted_proxies: Vec<Cidr>,
    unknown_peers: AtomicU64,
    unknown_peer_warned: Mutex<Option<Instant>>,
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
            git_principal: Arc::new(KeyedCounter::new()),
            git_replica: Arc::new(Semaphore::new(cfg.git.fetch_per_replica as usize)),
            semantic_replica: Arc::new(Semaphore::new(cfg.semantic_concurrency as usize)),
            service_tokens: HashSet::new(),
            ids: Arc::new(Identities {
                verified: Verified::new(VERIFIED_TTL, VERIFIED_MAX_AGE, MAX_VERIFIED),
                bootstrap: Bootstrap::new(max_keys.min(MAX_BOOTSTRAP)),
            }),
            bootstrap_gate: KeyedLimiter::bounded(
                BOOTSTRAP_PEER_RATE,
                BOOTSTRAP_PEER_BURST,
                max_keys,
                OnFull::Overflow,
            ),
            // `Limits::validate` rejects a bad entry; one that gets here anyway trusts nobody.
            trusted_proxies: cfg
                .trusted_proxies
                .iter()
                .filter_map(|p| p.parse().ok())
                .collect(),
            unknown_peers: AtomicU64::new(0),
            unknown_peer_warned: Mutex::new(None),
        }
    }

    /// Test hook: the lifetimes of a verified entry.
    #[cfg(test)]
    fn with_verified_ttl(self, ttl: Duration) -> Self {
        self.with_verified_lifetimes(ttl, VERIFIED_MAX_AGE)
    }

    #[cfg(test)]
    fn with_verified_lifetimes(mut self, ttl: Duration, max_age: Duration) -> Self {
        self.ids = Arc::new(Identities {
            verified: Verified::new(ttl, max_age, MAX_VERIFIED),
            bootstrap: Bootstrap::new(self.ids.bootstrap.max),
        });
        self
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
            ids: self.ids.clone(),
        })
    }

    /// The client address to charge: the TCP peer, or, when the peer is a trusted proxy, the
    /// rightmost `X-Forwarded-For` hop that is not itself a trusted proxy. The header is ignored
    /// from any other peer.
    ///
    /// Hops are read right to left, so only what the trusted proxies appended can matter: the
    /// first hop that is not a trusted proxy decides, and nothing to its left is looked at. A hop
    /// that does not parse before that point, a header with more than [`XFF_MAX_HOPS`] hops to
    /// get through, or one with no client in it, all give the proxy's own address. Only the last
    /// [`XFF_MAX_BYTES`] bytes of each header value are read.
    pub fn client_ip(&self, tcp: IpAddr, headers: &axum::http::HeaderMap) -> IpAddr {
        let trusted = |ip: IpAddr| self.trusted_proxies.iter().any(|c| c.contains(ip));
        if !trusted(tcp) {
            return tcp;
        }
        let mut seen = 0;
        for value in headers.get_all("x-forwarded-for").iter().rev() {
            let bytes = value.as_bytes();
            let cut = bytes.len() > XFF_MAX_BYTES;
            let tail = &bytes[bytes.len().saturating_sub(XFF_MAX_BYTES)..];
            // Split the raw bytes and decode only the hops that are examined: junk to the left of
            // the deciding hop, whatever its encoding, is never looked at. When the value was
            // cut, its first segment may be the end of a longer hop and is never read.
            let segments: Vec<&[u8]> = tail.split(|b| *b == b',').collect();
            for segment in segments[usize::from(cut)..].iter().rev() {
                seen += 1;
                if seen > XFF_MAX_HOPS {
                    return tcp;
                }
                let hop = std::str::from_utf8(segment)
                    .ok()
                    .and_then(|t| t.trim().parse::<IpAddr>().ok());
                match hop {
                    Some(ip) if !trusted(ip) => return ip,
                    Some(_) => {}
                    None => return tcp,
                }
            }
        }
        tcp
    }

    /// Requests that arrived with no peer address. They all share one bucket, so a non-zero
    /// count means the transport is not providing connection info.
    pub fn unknown_peers(&self) -> u64 {
        self.unknown_peers.load(Ordering::Relaxed)
    }

    fn note_unknown_peer(&self) {
        let total = self.unknown_peers.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self
            .unknown_peer_warned
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.is_none_or(|t| t.elapsed() >= UNKNOWN_PEER_WARN_EVERY) {
            *last = Some(Instant::now());
            tracing::warn!(
                total,
                "request without a peer address; all such callers share one admission bucket"
            );
        }
    }

    /// Who to charge for a request with this bearer `token` (if any) from `peer`.
    pub fn identify(&self, token: Option<&str>, peer: Option<IpAddr>) -> Caller {
        if let Some(t) = token {
            let h = token_hash(t);
            if self.service_tokens.contains(&h) {
                return Caller::Service;
            }
            if let Some(who) = self.ids.verified.lookup(&h) {
                return who;
            }
        }
        Caller::Unverified(Peer::from_ip(peer))
    }

    /// Requests currently admitted on this replica.
    pub fn in_flight(&self) -> usize {
        self.cfg.max_inflight as usize - self.inflight.available_permits()
    }

    /// Admits one call of `class` bearing `token` from `peer`: identifies the caller, admits it,
    /// and keeps a verified token verified while it is in use. A token that its peer's bucket has
    /// refused may still spend its bootstrap allowance (not for `Objects`, whose bytes need a
    /// bucket to be charged to).
    pub fn admit_request(
        &self,
        token: Option<&str>,
        peer: Option<IpAddr>,
        class: Class,
        cost: u64,
    ) -> Result<(Caller, Permit), Status> {
        if peer.is_none() {
            self.note_unknown_peer();
        }
        let who = self.identify(token, peer);
        let hash = token.map(token_hash);
        match self.try_admit(who, class, cost) {
            Ok(permit) => {
                if let (Caller::Verified(_), Some(h)) = (who, &hash) {
                    self.ids.verified.touch(h);
                }
                Ok((who, permit))
            }
            Err(refused) => {
                // Only a cheap call, and only when the peer's rate bucket is what said no: never
                // for overload, a concurrency cap or the heavier classes.
                if let (Caller::Unverified(peer), Some(token), true) =
                    (who, hash, refused.bucket && class == Class::Cheap)
                    && self.bootstrap_gate.check(&who, 1).is_ok()
                    && self.ids.bootstrap.take(&token)
                {
                    let who = Caller::Bootstrap { token, peer };
                    return self.admit(who, class, cost).map(|p| (who, p));
                }
                Err(refused.status)
            }
        }
    }

    /// Admits one call of `class` by `who`, charging `cost` (bytes for `Objects`, else ignored).
    /// Never waits: the error is the status to send back.
    pub fn admit(&self, who: Caller, class: Class, cost: u64) -> Result<Permit, Status> {
        self.try_admit(who, class, cost).map_err(|r| r.status)
    }

    /// The key a caller's in-flight slots are counted under. A bootstrapping token counts
    /// against its peer, so new tokens do not multiply the peer's slots.
    fn slot_key(who: Caller) -> Caller {
        match who {
            Caller::Bootstrap { peer, .. } => Caller::Unverified(peer),
            other => other,
        }
    }

    fn try_admit(&self, who: Caller, class: Class, cost: u64) -> Result<Permit, Refusal> {
        let mut permit = Permit::default();
        let slot = self
            .inflight
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refusal::other(overloaded()))?;
        permit.hold(slot);
        let max = self.cfg.max_inflight;
        if class == Class::Background
            && self.inflight.available_permits() < (max / BACKGROUND_FREE_SHARE) as usize
        {
            return Err(Refusal::other(overloaded()));
        }
        let slots = Self::slot_key(who);
        let token_slot = self
            .per_token
            .try_acquire(&slots, self.cfg.max_inflight_per_token)
            .map_err(|shed| Refusal::other(rate_limited(shed)))?;
        permit.hold(CounterHold(token_slot));

        let class = if who == Caller::Service {
            Class::Service
        } else {
            class
        };
        match class {
            _ if matches!(who, Caller::Bootstrap { .. }) => Ok(()),
            Class::Cheap | Class::Background => self.cheap.check(&who, 1),
            // `cost` is what is known up front; the body's bytes are charged as they stream.
            Class::Objects => self.objects.check(&who, cost),
            // A fetch's bytes are charged as they stream; here it costs one unit.
            Class::Git => self.objects.check(&who, 1),
            Class::UpdateRefs => self.update_refs.check(&who, 1),
            Class::Codeintel => self.codeintel.check(&who, 1),
            Class::Service => self.service.check(&who, 1),
            Class::Semantic => self.semantic.check(&who, 1),
            Class::Expensive => Ok(()),
        }
        .map_err(|shed| Refusal {
            status: rate_limited(shed),
            bucket: true,
        })?;

        match class {
            Class::Expensive => {
                permit.hold(CounterHold(
                    self.expensive_principal
                        .try_acquire(&slots, self.cfg.expensive_per_principal)
                        .map_err(|shed| Refusal::other(rate_limited(shed)))?,
                ));
                permit.hold(
                    self.replica_slot(&self.expensive_replica)
                        .map_err(Refusal::other)?,
                );
            }
            Class::Semantic => permit.hold(
                self.replica_slot(&self.semantic_replica)
                    .map_err(Refusal::other)?,
            ),
            Class::Git => {
                permit.hold(CounterHold(
                    self.git_principal
                        .try_acquire(&slots, self.cfg.git.fetch_per_principal)
                        .map_err(|shed| Refusal::other(rate_limited(shed)))?,
                ));
                permit.hold(
                    self.replica_slot(&self.git_replica)
                        .map_err(Refusal::other)?,
                );
            }
            _ => {}
        }
        Ok(permit)
    }

    /// Admits one git pack command that arrives on a stream transport (SSH, SPEC §25.11.7) from
    /// `principal`, a principal the authorizer has recognised. A fetch is the `git` class: one
    /// of `limits.git`'s per-principal and per-replica fetch slots, held until the permit is
    /// dropped. A push is a `cheap` call here; its own caps are the push slots (U82). The meter
    /// charges the command's bytes, in and out, to the principal's `objects` bucket as they move.
    pub(crate) fn admit_pack(
        &self,
        principal: &str,
        fetch: bool,
    ) -> Result<(Permit, PackMeter), Status> {
        let who = Caller::principal(principal);
        let class = if fetch { Class::Git } else { Class::Cheap };
        let permit = self.admit(who, class, 1)?;
        let meter = PackMeter {
            limiter: self.objects.clone(),
            who,
        };
        Ok((permit, meter))
    }

    /// The meter for the body bytes of an `objects` call, or `None` for any other call.
    fn meter(&self, who: Caller, class: Class) -> Option<Meter> {
        (matches!(class, Class::Objects | Class::Git) && who != Caller::Service).then(|| Meter {
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

/// Charges the bytes of one git pack command on a stream transport to its principal.
#[derive(Clone)]
pub(crate) struct PackMeter {
    limiter: Arc<Tiered>,
    who: Caller,
}

impl PackMeter {
    /// Takes `bytes` from the principal's `objects` bucket; the error is the status to give back.
    pub(crate) fn charge(&self, bytes: u64) -> Result<(), Status> {
        self.limiter
            .charge_at(&self.who, bytes, Instant::now())
            .map_err(rate_limited)
    }
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
                    && let Err(shed) = self.meter.limiter.charge_at(
                        &self.meter.who,
                        data.len() as u64,
                        Instant::now(),
                    )
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
/// an `objects` call it also paces the bytes it sends to the caller's byte bucket: a frame that
/// the bucket cannot cover is held back until it can, then charged in full (as debt if it is
/// larger than a burst). That throttles the stream's bandwidth instead of cutting it. Each wait
/// is at most the time to refill one burst.
struct Held {
    inner: Body,
    meter: Option<Meter>,
    /// A frame already read from `inner` and waiting for the bucket.
    parked: Option<Frame<Bytes>>,
    pause: Option<Pin<Box<tokio::time::Sleep>>>,
    _permit: Permit,
}

impl HttpBody for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        let Some(meter) = &this.meter else {
            return Pin::new(&mut this.inner).poll_frame(cx);
        };
        loop {
            if let Some(pause) = this.pause.as_mut() {
                std::task::ready!(pause.as_mut().poll(cx));
                this.pause = None;
            }
            if this.parked.is_none() {
                match Pin::new(&mut this.inner).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => this.parked = Some(frame),
                    other => return other,
                }
            }
            let bytes = this
                .parked
                .as_ref()
                .and_then(Frame::data_ref)
                .map_or(0, |d| d.len() as u64);
            if bytes == 0 {
                return Poll::Ready(this.parked.take().map(Ok));
            }
            let now = tokio::time::Instant::now().into_std();
            match meter.limiter.charge_at(&meter.who, bytes, now) {
                Ok(()) => return Poll::Ready(this.parked.take().map(Ok)),
                Err(shed) => this.pause = Some(Box::pin(tokio::time::sleep(shed.retry_after))),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.parked.is_none() && self.inner.is_end_stream()
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
        let peer =
            connect_addr(req.extensions()).map(|tcp| self.admission.client_ip(tcp, req.headers()));
        let scope = Peer::from_ip(peer);
        let path = req.uri().path();
        let git = class_of(path).is_none();
        let Some(class) = class_of(path).or_else(|| git_class_of(path)) else {
            return Box::pin(with_peer(scope, inner.call(req)));
        };
        // The size of the body is not known yet and the header cannot be trusted, so the call
        // costs one unit here and the body's real bytes are charged as they stream.
        match self.admission.admit_request(
            credential_secret(req.headers()).as_deref(),
            peer,
            class,
            1,
        ) {
            Err(status) => Box::pin(std::future::ready(Ok(if git {
                git_refusal(&status)
            } else {
                status.into_http()
            }))),
            Ok((who, permit)) => {
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
                Box::pin(with_peer(scope, async move {
                    let resp = inner.call(req).await?;
                    Ok(resp.map(|body| {
                        Body::new(Held {
                            inner: body,
                            meter,
                            parked: None,
                            pause: None,
                            _permit: permit,
                        })
                    }))
                }))
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
        assert_eq!(
            class_of("/asc.v1.CodeIntel/Semantic"),
            Some(Class::Semantic)
        );
        assert_eq!(class_of("/asc.v1.CodeIntel/Search"), Some(Class::Codeintel));
        assert_eq!(
            class_of("/asc.v1.CodeIntel/IndexStatus"),
            Some(Class::Cheap)
        );
        assert_eq!(
            class_of("/asc.v1.Cloud/UpdateWorkspace"),
            Some(Class::Background)
        );
        assert_eq!(class_of("/asc.v1.Cloud/GetWorkspace"), Some(Class::Cheap));
        assert_eq!(
            class_of("/asc.v1.Changes/InterDiff"),
            Some(Class::Expensive)
        );
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
    fn git_paths_have_a_class_and_nothing_else_does() {
        for (path, class) in [
            ("/acme/web.git/info/refs", Class::Cheap),
            ("/acme/web/info/refs", Class::Cheap),
            ("/acme/web.git/git-upload-pack", Class::Git),
            ("/acme/web/git-upload-pack", Class::Git),
            ("/acme/web.git/git-receive-pack", Class::Cheap),
        ] {
            assert_eq!(git_class_of(path), Some(class), "{path}");
        }
        for path in [
            "/",
            "/healthz",
            "/acme/web",
            "/acme/web/HEAD",
            "/acme/web/info/refs/",
            "/acme/web/info/x",
            "/acme/web/git-upload-pack/x",
            "/seeds/acme/web/latest",
            "/v8/artifacts/abc",
            "/asc.v1.Repo/Version",
        ] {
            assert_eq!(git_class_of(path), None, "{path}");
        }
        // gRPC paths keep their own classes.
        assert_eq!(class_of("/acme/web/git-upload-pack"), None);
    }

    #[test]
    fn fetches_are_capped_per_principal_and_per_replica_from_limits_git() {
        let a = Admission::new(&limits(|l| {
            l.git.fetch_per_principal = 2;
            l.git.fetch_per_replica = 3;
        }));
        let (x, y) = (caller(1), caller(2));
        let _a1 = a.admit(x, Class::Git, 1).unwrap();
        let _a2 = a.admit(x, Class::Git, 1).unwrap();
        assert!(is_rate_limited(&a.admit(x, Class::Git, 1).err().unwrap()));
        let _b1 = a.admit(y, Class::Git, 1).unwrap();
        // Three fetches run on the replica, so a fourth principal is shed.
        let err = a.admit(caller(3), Class::Git, 1).err().unwrap();
        assert!(retry_after(&err).is_some());
        assert_eq!(a.in_flight(), 3, "a shed fetch leaves nothing held");
        // Discovery is `cheap` and does not use a fetch slot.
        a.admit(x, Class::Cheap, 1).unwrap();
        // The refusal reaches a git client as plain HTTP with whole seconds of Retry-After.
        let res = git_refusal(&err);
        assert!(matches!(res.status().as_u16(), 429 | 503));
        assert!(res.headers().contains_key(axum::http::header::RETRY_AFTER));
    }

    #[test]
    fn a_fetch_streams_its_bytes_through_the_objects_bucket() {
        let a = Admission::new(&limits(|l| l.objects_bytes_per_sec = ByteSize(1000)));
        let who = caller(1);
        assert!(a.meter(who, Class::Git).is_some());
        assert!(a.meter(who, Class::Objects).is_some());
        assert!(a.meter(who, Class::Cheap).is_none());
        assert!(a.meter(Caller::Service, Class::Git).is_none());
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

    /// Answers with the peer the handler would see through [`current_peer`].
    #[derive(Clone)]
    struct SeesPeer;

    impl Service<Request<Body>> for SeesPeer {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = BoxFuture<'static, Result<Response<Body>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<Body>) -> Self::Future {
            Box::pin(async { Ok(Response::new(Body::from(format!("{:?}", current_peer())))) })
        }
    }

    #[tokio::test]
    async fn a_handler_sees_the_client_peer_the_layer_charged() {
        let admission = Arc::new(Admission::new(&Limits::default()));
        let mut svc = AdmissionLayer::new(admission).layer(SeesPeer);
        for path in [
            "/asc.v1.Repo/WatchRefs",
            "/asc.v1.Repo/Version",
            "/not-asc/at-all",
        ] {
            let req = request(path, "t", 7, Body::empty());
            let resp = svc.call(req).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
            let want = format!("{:?}", Peer::from_ip(Some(IpAddr::from([10, 0, 0, 7]))));
            assert_eq!(String::from_utf8_lossy(&body), want, "{path}");
        }
        assert_eq!(current_peer(), Peer::Unknown, "outside a request");
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
        // The peer's burst of 5, plus the small per-peer bootstrap burst (8) that lets a real
        // token reach the authorizer; 200 tokens do not get 200 buckets.
        assert!(
            admitted <= 5 + BOOTSTRAP_PEER_BURST as usize + 1,
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

    /// A call carrying `authorization` verbatim, from peer `from`.
    fn call_with_header<S>(
        svc: &mut AdmissionService<S>,
        authorization: &str,
        from: u8,
    ) -> Response<Body>
    where
        S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        let mut req = Request::builder()
            .uri("/asc.v1.Repo/Version")
            .header("authorization", authorization)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(peer(from));
        futures::executor::block_on(svc.call(req)).unwrap()
    }

    // U78: the verified-token record keys on the credential's secret, whatever the scheme, so a
    // Basic login is charged as its principal just as a Bearer one is.
    #[tokio::test]
    async fn a_basic_credential_the_authorizer_accepted_is_charged_as_its_principal() {
        use base64::Engine as _;
        let basic = |user: &str, secret: &str| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"))
            )
        };
        let admission = Arc::new(Admission::new(&limits(|l| {
            l.cheap_burst = 3;
            l.cheap_rps = 1;
        })));
        let mut svc = AdmissionLayer::new(admission.clone()).layer(Echo);
        let authz = admission.recording(Arc::new(Accepts));
        let alice = basic("x-access-token", "alice-token");
        for _ in 0..10 {
            call_with_header(&mut svc, &alice, 1);
        }
        assert!(shed(&call_with_header(&mut svc, &alice, 1)));
        // A refused Basic password gains nothing, whatever the user name says.
        let wrong = Credential::basic("alice", "alice-token-but-wrong").unwrap();
        assert!(
            authz
                .authorize_credential(Some(&wrong), "o/r", Access::Read)
                .await
                .is_err()
        );
        assert!(shed(&call_with_header(
            &mut svc,
            &basic("alice", "alice-token-but-wrong"),
            1
        )));
        // An accepted one is charged as alice from any peer, in any scheme.
        let good = Credential::basic("x-access-token", "alice-token").unwrap();
        let who = authz
            .authorize_credential(Some(&good), "o/r", Access::Read)
            .await
            .unwrap();
        assert_eq!(who.id, "alice");
        for _ in 0..3 {
            assert!(!shed(&call_with_header(&mut svc, &alice, 2)));
        }
        assert!(shed(&call_with_header(&mut svc, &alice, 3)));
        assert!(shed(&call_with_header(&mut svc, "Bearer alice-token", 4)));
        assert!(shed(&call_with_header(&mut svc, "token alice-token", 5)));
        // The token as the user name with an empty password (`https://TOKEN@host`) is the same.
        assert!(shed(&call_with_header(
            &mut svc,
            &basic("alice-token", ""),
            6
        )));
        // The inner authorizer sees the credential, not just the secret.
        assert_eq!(
            admission.identify(Some("alice-token"), None),
            Caller::principal("alice")
        );
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
            let mut req = request("/asc.v1.Repo/Version", "", 1, Body::empty());
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

    /// Answers every call with a body of `.0` bytes in frames of `.1` bytes.
    #[derive(Clone)]
    struct Download(usize, usize);

    impl Service<Request<Body>> for Download {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Response<Body>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<Body>) -> Self::Future {
            std::future::ready(Ok(Response::new(chunks(self.0 / self.1, self.1))))
        }
    }

    // A response stream is held to the byte budget: it takes as long as its size needs.
    #[tokio::test]
    async fn a_response_stream_is_paced_to_the_byte_budget() {
        // 30 000 bytes in 5 000-byte frames on a 20 000 B/s budget: the first burst is free and
        // the other 10 000 bytes take half a second.
        let mut svc = AdmissionLayer::new(byte_budget(20_000)).layer(Download(30_000, 5_000));
        let start = Instant::now();
        let mut resp = call(&mut svc, "/asc.v1.Repo/GetObjects", "t");
        assert!(!shed(&resp));
        let mut sent = 0;
        while let Some(f) =
            std::future::poll_fn(|cx| Pin::new(resp.body_mut()).poll_frame(cx)).await
        {
            sent += f.unwrap().data_ref().map_or(0, Bytes::len);
        }
        assert_eq!(sent, 30_000, "the stream is throttled, not cut");
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(450), "took only {took:?}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
        // A cheap call is a different bucket.
        assert!(!shed(&call(&mut svc, "/asc.v1.Repo/Version", "t")));
    }

    // An upload frame larger than the bucket is charged in full, as debt.
    #[test]
    fn an_upload_frame_larger_than_the_bucket_costs_its_full_size() {
        let mut svc = AdmissionLayer::new(byte_budget(1000)).layer(Drain);
        let big = put(&mut svc, None, chunks(1, 3000));
        assert_eq!(header(&big, "x-failed"), "false");
        assert_eq!(header(&big, "x-bytes"), "3000");
        // 2 000 bytes of debt: the next call is refused for about 2 s, not about 0.
        let next = put(&mut svc, None, chunks(1, 10));
        assert!(shed(&next));
        let wait: u64 = header(&next, RETRY_AFTER_MS).parse().unwrap();
        assert!(wait >= 1500, "retry-after was {wait} ms");
    }

    fn drain_peer(a: &Admission, ip: IpAddr) {
        while a.admit_request(None, Some(ip), Class::Cheap, 1).is_ok() {}
    }

    // Verification happens after admission, so a saturated peer must not block it.
    #[tokio::test]
    async fn a_new_token_behind_a_saturated_peer_can_reach_the_authorizer_and_be_verified() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 3;
            l.cheap_rps = 1;
        }));
        let authz = a.recording(Arc::new(Accepts));
        let ip: IpAddr = [10, 1, 1, 1].into();
        drain_peer(&a, ip);
        assert!(a.admit_request(None, Some(ip), Class::Cheap, 1).is_err());
        // The token is not verified and its peer is spent, but it has a small allowance.
        let mut through = 0;
        for _ in 0..10 {
            if let Ok((who, _)) = a.admit_request(Some("bob-token"), Some(ip), Class::Cheap, 1) {
                assert!(matches!(who, Caller::Bootstrap { .. }));
                through += 1;
            }
        }
        assert_eq!(through, BOOTSTRAP_ALLOWANCE);
        // Fresh allowance is not the point: once the authorizer accepts the token it is its own
        // caller, whatever the peer's state.
        let bob = BearerToken::new("bob-token").unwrap();
        authz
            .authorize(Some(&bob), "o/r", Access::Read)
            .await
            .unwrap();
        let (who, _) = a
            .admit_request(Some("bob-token"), Some(ip), Class::Cheap, 1)
            .unwrap();
        assert!(matches!(who, Caller::Verified(_)));
    }

    #[tokio::test]
    async fn a_refused_token_gets_no_bootstrap_and_random_tokens_get_a_bounded_few() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 3;
            l.cheap_rps = 1;
        }));
        let authz = a.recording(Arc::new(Accepts));
        let ip: IpAddr = [10, 2, 2, 2].into();
        drain_peer(&a, ip);
        // The authorizer refuses this token, so it is charged to the peer: nothing gets through.
        let evil = BearerToken::new("evil").unwrap();
        assert!(
            authz
                .authorize(Some(&evil), "o/r", Access::Read)
                .await
                .is_err()
        );
        for _ in 0..5 {
            assert!(
                a.admit_request(Some("evil"), Some(ip), Class::Cheap, 1)
                    .is_err()
            );
        }
        // A flood of fresh random tokens from the one saturated peer: only the peer's bootstrap
        // burst gets through.
        let through = (0..500)
            .filter(|n| {
                let t = format!("random-{n}");
                a.admit_request(Some(&t), Some(ip), Class::Cheap, 1).is_ok()
            })
            .count();
        assert!(
            through <= BOOTSTRAP_PEER_BURST as usize + 1,
            "{through} got through"
        );
    }

    #[tokio::test]
    async fn steady_use_keeps_a_token_verified_past_its_ttl_while_the_peer_is_saturated() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 50;
            l.cheap_rps = 1;
        }))
        .with_verified_ttl(Duration::from_millis(300));
        let authz = a.recording(Arc::new(Accepts));
        let ip: IpAddr = [10, 3, 3, 3].into();
        let alice = BearerToken::new("alice-token").unwrap();
        authz
            .authorize(Some(&alice), "o/r", Access::Read)
            .await
            .unwrap();
        drain_peer(&a, ip);
        // Ten requests over 800 ms, more than twice the TTL, each well inside it.
        for i in 0..10 {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let (who, _) = a
                .admit_request(Some("alice-token"), Some(ip), Class::Cheap, 1)
                .unwrap_or_else(|e| panic!("request {i} was refused: {e}"));
            assert!(matches!(who, Caller::Verified(_)), "request {i}: {who:?}");
        }
        // Left alone for longer than the TTL, the entry lapses.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let who = a.identify(Some("alice-token"), Some(ip));
        assert!(matches!(who, Caller::Unverified(_)));
    }

    fn forwarded(chain: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", chain.parse().unwrap());
        h
    }

    fn with_proxies(proxies: &[&str]) -> Admission {
        Admission::new(&limits(|l| {
            l.trusted_proxies = proxies.iter().map(ToString::to_string).collect();
        }))
    }

    #[test]
    fn x_forwarded_for_is_honoured_only_from_a_trusted_proxy() {
        let a = with_proxies(&["10.0.0.0/24", "fd00::/8"]);
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // Not a trusted peer: the header is ignored, whatever it says.
        let h = forwarded("203.0.113.5");
        assert_eq!(a.client_ip(ip("192.0.2.9"), &h), ip("192.0.2.9"));
        // A trusted proxy: the rightmost hop that is not a proxy.
        assert_eq!(a.client_ip(ip("10.0.0.1"), &h), ip("203.0.113.5"));
        let h = forwarded("1.1.1.1, 203.0.113.5, 10.0.0.7");
        assert_eq!(a.client_ip(ip("10.0.0.1"), &h), ip("203.0.113.5"));
        // No header, or one that does not parse, falls back to the proxy itself.
        let none = axum::http::HeaderMap::new();
        assert_eq!(a.client_ip(ip("10.0.0.1"), &none), ip("10.0.0.1"));
        assert_eq!(
            a.client_ip(ip("10.0.0.1"), &forwarded("not-an-ip")),
            ip("10.0.0.1")
        );
        // With no proxies configured, nobody is trusted.
        let off = with_proxies(&[]);
        assert_eq!(off.client_ip(ip("10.0.0.1"), &h), ip("10.0.0.1"));
    }

    #[test]
    fn a_forwarded_header_from_an_untrusted_peer_does_not_change_the_bucket() {
        let admission = Arc::new(Admission::new(&limits(|l| {
            l.cheap_burst = 5;
            l.cheap_rps = 1;
            l.trusted_proxies = vec!["10.9.9.0/24".into()];
        })));
        let mut svc = AdmissionLayer::new(admission).layer(Echo);
        // Peer 10.0.0.1 is not a proxy; it claims a different client in every request.
        let admitted = (0..100)
            .filter(|n| {
                let mut req = request("/asc.v1.Repo/Version", "", 1, Body::empty());
                req.headers_mut().insert(
                    "x-forwarded-for",
                    format!("198.51.100.{n}").parse().unwrap(),
                );
                !shed(&futures::executor::block_on(svc.call(req)).unwrap())
            })
            .count();
        assert_eq!(admitted, 5);
    }

    #[test]
    fn clients_behind_a_trusted_proxy_are_told_apart() {
        let admission = Arc::new(Admission::new(&limits(|l| {
            l.cheap_burst = 2;
            l.cheap_rps = 1;
            l.trusted_proxies = vec!["10.0.0.0/24".into()];
        })));
        let mut svc = AdmissionLayer::new(admission).layer(Echo);
        let mut go = |client: &str| {
            let mut req = request("/asc.v1.Repo/Version", "", 1, Body::empty());
            req.headers_mut()
                .insert("x-forwarded-for", client.parse().unwrap());
            shed(&futures::executor::block_on(svc.call(req)).unwrap())
        };
        assert!(!go("203.0.113.1"));
        assert!(!go("203.0.113.1"));
        assert!(go("203.0.113.1"));
        assert!(!go("203.0.113.2"), "another client has its own bucket");
    }

    #[test]
    fn cidr_ranges_parse_and_match_both_families() {
        let c: Cidr = "10.1.0.0/16".parse().unwrap();
        assert!(c.contains("10.1.200.3".parse().unwrap()));
        assert!(!c.contains("10.2.0.1".parse().unwrap()));
        let host: Cidr = "192.0.2.7".parse().unwrap();
        assert!(host.contains("192.0.2.7".parse().unwrap()));
        assert!(!host.contains("192.0.2.8".parse().unwrap()));
        let v6: Cidr = "2001:db8::/32".parse().unwrap();
        assert!(v6.contains("2001:db8:5::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("not-an-ip".parse::<Cidr>().is_err());
        assert!("10.0.0.0/x".parse::<Cidr>().is_err());
    }

    #[test]
    fn a_request_without_a_peer_address_is_counted() {
        let a = Admission::new(&limits(|_| {}));
        assert_eq!(a.unknown_peers(), 0);
        a.admit_request(None, None, Class::Cheap, 1).unwrap();
        a.admit_request(None, Some([10, 0, 0, 1].into()), Class::Cheap, 1)
            .unwrap();
        assert_eq!(a.unknown_peers(), 1);
    }

    #[test]
    fn the_peer_is_read_from_tonic_connection_info() {
        let mut req = request("/asc.v1.Repo/Version", "", 7, Body::empty());
        assert_eq!(
            connect_addr(req.extensions()),
            Some(IpAddr::from([10, 0, 0, 7]))
        );
        req.extensions_mut().clear();
        assert_eq!(connect_addr(req.extensions()), None);
    }

    #[test]
    fn junk_left_of_the_client_hop_cannot_change_who_the_client_is() {
        let a = with_proxies(&["10.0.0.0/24"]);
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let proxy = ip("10.0.0.1");
        let client = ip("203.0.113.9");
        let from = |chain: &str| a.client_ip(proxy, &forwarded(chain));
        assert_eq!(from("garbage, 203.0.113.9"), client);
        assert_eq!(from("1.2.3.4, junk, 203.0.113.9, 10.0.0.7"), client);
        assert_eq!(from(&format!("{}203.0.113.9", "x,".repeat(500))), client);
        // Junk right of the client, next to the proxies, is not the client: use the proxy.
        assert_eq!(from("203.0.113.9, garbage"), proxy);
        assert_eq!(from("203.0.113.9, 10.0.0.7, garbage"), proxy);
        // Only trusted hops: no client in the header.
        assert_eq!(from("10.0.0.5, 10.0.0.7"), proxy);
        // Too many hops to get through, or a value too long to read whole.
        let many = "10.0.0.5,".repeat(40) + "203.0.113.9";
        assert_eq!(from(&many), client, "the client is the rightmost hop");
        let buried = format!("203.0.113.9{}", ",10.0.0.5".repeat(40));
        assert_eq!(from(&buried), proxy);
        let long = format!("{}10.0.0.5", "7".repeat(5000));
        assert_eq!(from(&long), proxy);
        // Several header lines are one list; the last line is the rightmost.
        let mut h = forwarded("198.51.100.1");
        h.append("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert_eq!(a.client_ip(proxy, &h), client);
    }

    struct Switch(std::sync::atomic::AtomicBool);

    #[async_trait]
    impl Authorizer for Switch {
        async fn authorize(
            &self,
            token: Option<&BearerToken>,
            _repo: &str,
            _access: Access,
        ) -> Result<Principal, AuthError> {
            match token {
                Some(_) if self.0.load(Ordering::SeqCst) => Ok(Principal { id: "alice".into() }),
                _ => Err(AuthError::Forbidden),
            }
        }
    }

    #[tokio::test]
    async fn a_refused_token_loses_its_verified_bucket() {
        let a = Admission::new(&limits(|_| {}));
        let switch = Arc::new(Switch(std::sync::atomic::AtomicBool::new(true)));
        let authz = a.recording(switch.clone());
        let token = BearerToken::new("alice-token").unwrap();
        authz
            .authorize(Some(&token), "o/r", Access::Read)
            .await
            .unwrap();
        let ip = Some(IpAddr::from([10, 4, 4, 4]));
        assert!(matches!(
            a.identify(Some("alice-token"), ip),
            Caller::Verified(_)
        ));
        // The token is revoked. The next refusal drops it to the peer tier, and use cannot
        // bring it back.
        switch.0.store(false, Ordering::SeqCst);
        assert!(
            authz
                .authorize(Some(&token), "o/r", Access::Read)
                .await
                .is_err()
        );
        assert!(matches!(
            a.identify(Some("alice-token"), ip),
            Caller::Unverified(_)
        ));
        assert!(
            a.admit_request(Some("alice-token"), ip, Class::Cheap, 1)
                .is_ok()
        );
        assert!(matches!(
            a.identify(Some("alice-token"), ip),
            Caller::Unverified(_)
        ));
    }

    #[tokio::test]
    async fn constant_use_cannot_keep_a_token_verified_past_the_absolute_cap() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 100;
            l.cheap_rps = 1;
        }))
        .with_verified_lifetimes(Duration::from_millis(300), Duration::from_millis(500));
        let authz = a.recording(Arc::new(Accepts));
        let alice = BearerToken::new("alice-token").unwrap();
        authz
            .authorize(Some(&alice), "o/r", Access::Read)
            .await
            .unwrap();
        let ip = Some(IpAddr::from([10, 5, 5, 5]));
        let start = Instant::now();
        let mut lapsed = None;
        for _ in 0..25 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let verified = matches!(
                a.admit_request(Some("alice-token"), ip, Class::Cheap, 1),
                Ok((Caller::Verified(_), _))
            );
            if !verified {
                lapsed = Some(start.elapsed());
                break;
            }
        }
        // Used every 60 ms, well inside the 300 ms lifetime, it still ends at the 500 ms cap.
        let lapsed = lapsed.expect("a token in constant use stayed verified past the cap");
        assert!(lapsed >= Duration::from_millis(400), "lapsed at {lapsed:?}");
        assert!(lapsed < Duration::from_millis(900), "lapsed at {lapsed:?}");
    }

    #[test]
    fn a_bootstrap_caller_cannot_start_an_expensive_call() {
        let a = Admission::new(&limits(|l| l.expensive_per_principal = 1));
        let ip = Some(IpAddr::from([10, 6, 6, 6]));
        // The peer's one expensive slot is taken. A new token does not get one of its own.
        let held = a
            .admit_request(Some("t1"), ip, Class::Expensive, 1)
            .unwrap();
        assert!(
            a.admit_request(Some("t2"), ip, Class::Expensive, 1)
                .is_err()
        );
        assert!(
            a.admit_request(Some("t3"), ip, Class::Expensive, 1)
                .is_err()
        );
        assert_eq!(a.in_flight(), 1);
        drop(held);
        // Nor does the class that needs a byte bucket or a replica slot.
        let ip = Some(IpAddr::from([10, 6, 6, 7]));
        let a = Admission::new(&limits(|l| {
            l.semantic_concurrency = 1;
            l.semantic_rps = 1;
        }));
        let _s = a.admit_request(Some("s1"), ip, Class::Semantic, 1).unwrap();
        assert!(a.admit_request(Some("s2"), ip, Class::Semantic, 1).is_err());
    }

    #[test]
    fn bootstrap_concurrency_is_bounded_per_peer() {
        let a = Admission::new(&limits(|l| {
            l.cheap_burst = 3;
            l.cheap_rps = 1;
            l.max_inflight_per_token = 2;
        }));
        let ip = IpAddr::from([10, 7, 7, 7]);
        drain_peer(&a, ip);
        let token = |t: &'static str| a.admit_request(Some(t), Some(ip), Class::Cheap, 1);
        let one = token("tA").unwrap();
        let two = token("tB").unwrap();
        assert!(matches!(one.0, Caller::Bootstrap { .. }));
        // New tokens do not bring new slots: the peer has two in flight.
        assert!(token("tC").is_err());
        assert!(token("tD").is_err());
        drop(one);
        assert!(token("tC").is_ok());
        drop(two);
    }

    #[test]
    fn global_overload_gives_a_new_token_no_bootstrap() {
        let a = Admission::new(&limits(|l| {
            l.max_inflight = 2;
            l.cheap_burst = 3;
            l.cheap_rps = 1;
        }));
        let ip = IpAddr::from([10, 8, 8, 8]);
        drain_peer(&a, ip);
        let (p1, p2) = (
            a.admit(caller(1), Class::Cheap, 1).unwrap(),
            a.admit(caller(2), Class::Cheap, 1).unwrap(),
        );
        for _ in 0..5 {
            let refused = a
                .admit_request(Some("tok"), Some(ip), Class::Cheap, 1)
                .err()
                .unwrap();
            assert_eq!(refused.code(), Code::Unavailable);
        }
        drop((p1, p2));
        // Overload spent nothing of the token's allowance.
        let admitted = (0..10)
            .filter(|_| {
                a.admit_request(Some("tok"), Some(ip), Class::Cheap, 1)
                    .is_ok()
            })
            .count();
        assert_eq!(admitted, BOOTSTRAP_ALLOWANCE as usize);
    }

    #[test]
    fn junk_bytes_left_of_the_client_hop_are_never_decoded() {
        let a = with_proxies(&["10.0.0.0/24"]);
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        let client: IpAddr = "203.0.113.9".parse().unwrap();
        let header = |bytes: &[u8]| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                "x-forwarded-for",
                axum::http::HeaderValue::from_bytes(bytes).unwrap(),
            );
            h
        };
        // Not UTF-8 at all, left of the client.
        let h = header(b"\xff\xfe, 203.0.113.9");
        assert_eq!(a.client_ip(proxy, &h), client);
        // A multi-byte character cut in half by the 2 KiB window.
        let long = format!("{}, 203.0.113.9", "\u{e9}".repeat(2000));
        let h = header(long.as_bytes());
        assert_eq!(a.client_ip(proxy, &h), client);
        let odd = format!("x{}, 203.0.113.9", "\u{e9}".repeat(2000));
        let h = header(odd.as_bytes());
        assert_eq!(a.client_ip(proxy, &h), client);
        // Junk right of the client is still adjacent to the proxy.
        let h = header(b"203.0.113.9, \xff");
        assert_eq!(a.client_ip(proxy, &h), proxy);
    }

    #[test]
    fn pruning_a_full_verified_table_is_linear() {
        let max = 100_000;
        let v = Verified::new(Duration::from_secs(60), VERIFIED_MAX_AGE, max);
        for n in 0..max {
            v.record(&format!("token-{n}"), "alice");
        }
        // The table is full of live entries: recording one more prunes, finds nothing to drop,
        // and refuses. That has to be a single pass.
        let start = Instant::now();
        v.record("one-more", "bob");
        let took = start.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "pruning {max} entries took {took:?}"
        );
        assert!(v.lookup(&token_hash("one-more")).is_none());
        assert!(v.lookup(&token_hash("token-7")).is_some());
    }
}
