//! Turning a stored target into a working store, and verifying that it works.
//!
//! A target becomes a [`Live`] store in four steps, none of which a user can skip: the
//! credentials are opened with the vault (bound to the target's owner and id), the endpoint is
//! resolved and judged by the network guard, the connection is pinned to the vetted addresses,
//! and the S3 client is built from explicit settings (never from the environment). The store
//! is rebuilt when the target changes and re-resolved every few minutes, so a rebound name is
//! judged again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use asc_store::s3::{TargetConfig, TargetCredentials};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use super::db::Db;
use super::model::{Credentials, PROBE_DIR, State, Target, secret_context};
use super::routed::{Member, RoutedStore};
use crate::netguard::{BlockReason, Blocked, Guard, Shape};
use crate::registry::Ns;
use crate::vault::{Vault, VaultError};

/// How long a target's row is trusted before it is read again.
const META_TTL: Duration = Duration::from_secs(2);

/// Why a target cannot be used.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RouteError {
    #[error("the storage target is not enabled (it is {0})")]
    NotEnabled(&'static str),
    #[error("the storage target no longer exists")]
    Gone,
    #[error("the storage endpoint is blocked ({})", .0.slug())]
    Blocked(BlockReason),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error("{0}")]
    Other(String),
}

impl From<Blocked> for RouteError {
    fn from(b: Blocked) -> Self {
        Self::Blocked(b.reason())
    }
}

impl From<sqlx::Error> for RouteError {
    fn from(_: sqlx::Error) -> Self {
        Self::Other("storage database error".into())
    }
}

/// A fixed phrase for an object store error. The provider's own text can echo URLs, request ids
/// and bodies, so it is read here to pick a phrase and never copied.
pub fn sanitize(e: &object_store::Error) -> &'static str {
    use object_store::Error as E;
    let mut hay = format!("{e:?}").to_ascii_lowercase();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        hay.push(' ');
        hay.push_str(&s.to_string().to_ascii_lowercase());
        src = s.source();
    }
    // A provider reports a key it does not know, a bad signature and an expired token as a 403
    // like a missing permission; the code in the body tells them apart.
    if [
        "invalidaccesskeyid",
        "signaturedoesnotmatch",
        "invalidtoken",
        "expiredtoken",
        "invalidsecurity",
    ]
    .iter()
    .any(|t| hay.contains(t))
    {
        return "the provider rejected the credentials";
    }
    match e {
        E::NotFound { .. } => "the bucket or object was not found",
        E::Unauthenticated { .. } => "the provider rejected the credentials",
        E::PermissionDenied { .. } => "access denied: check the key's permissions on the bucket",
        E::AlreadyExists { .. } => "the object already exists",
        E::NotSupported { .. } | E::NotImplemented { .. } => {
            "the provider does not support this operation"
        }
        E::Precondition { .. } | E::NotModified { .. } => "the provider refused a condition",
        _ => classify_text(&hay),
    }
}

fn classify_text(hay: &str) -> &'static str {
    let has = |t: &str| hay.contains(t);
    if has("certificate") || has("tls") || has("ssl") || has("handshake") {
        "the TLS connection failed: check the endpoint certificate"
    } else if has("timed out") || has("timeout") || has("deadline") {
        "the request timed out"
    } else if has("quota") || has("507") || has("insufficient") {
        "the provider's storage quota is exceeded"
    } else if has("403") || has("access denied") || has("accessdenied") || has("forbidden") {
        "access denied: check the key's permissions on the bucket"
    } else if has("401") || has("invalidaccesskey") || has("signaturedoesnotmatch") {
        "the provider rejected the credentials"
    } else if has("nosuchbucket") || has("404") {
        "the bucket or object was not found"
    } else if has("429") || has("slowdown") || has("slow down") || has("503") || has("throttl") {
        "the provider is rate limiting or unavailable"
    } else if has("dns") || has("refused") || has("connect") || has("unreachable") || has("resolve")
    {
        "the endpoint could not be reached"
    } else {
        "the provider returned an unexpected response"
    }
}

/// A target ready to use.
pub struct Live {
    pub id: String,
    /// The target row's version this was built from.
    pub version: i64,
    pub store: Arc<dyn ObjectStore>,
    /// The target's prefix (`""` or `a/b`).
    pub base: Path,
    /// The addresses the guard vetted and the client is pinned to.
    addrs: Vec<std::net::IpAddr>,
    /// When the addresses were last judged.
    checked: Mutex<Instant>,
}

impl Live {
    fn age(&self) -> Duration {
        self.checked.lock().map_or(Duration::MAX, |c| c.elapsed())
    }

    fn touch(&self) {
        if let Ok(mut c) = self.checked.lock() {
            *c = Instant::now();
        }
    }

    /// Where the namespace `ns` lives in this target.
    pub fn ns_root(&self, ns: Ns) -> Path {
        self.base.clone().join("ns").join(ns.hex())
    }

    pub fn member(&self, ns: Ns) -> Member {
        Member {
            store: self.store.clone(),
            root: self.ns_root(ns),
        }
    }
}

/// Builds stores for targets. Shared by the router and the verification handshake.
#[derive(Clone)]
pub struct Builder {
    pub guard: Guard,
    pub vault: Arc<Vault>,
}

impl Builder {
    /// Opens the credentials and assembles the S3 settings. No network is touched.
    pub fn config(&self, t: &Target, sealed: &[u8]) -> Result<TargetConfig, RouteError> {
        let scope = t.owner.scope();
        let plain = self.vault.open(&secret_context(&scope, &t.id), sealed)?;
        let creds = Credentials::unpack(&plain).map_err(RouteError::Other)?;
        Ok(TargetConfig {
            endpoint: t.endpoint.clone(),
            region: t.region.clone(),
            bucket: t.bucket.clone(),
            prefix: t.prefix.clone(),
            addressing: t.addressing.to_store(),
            credentials: TargetCredentials {
                access_key_id: creds.access_key_id.to_string(),
                secret_access_key: creds.secret_access_key.to_string(),
                session_token: creds.session_token.as_ref().map(|s| s.to_string()),
            },
            allow_http: t.endpoint.starts_with("http://"),
        })
    }

    /// Resolves and pins the host requests go to (the bucket subdomain for virtual-host style).
    pub async fn pin(&self, cfg: &TargetConfig) -> Result<crate::netguard::Pinned, RouteError> {
        let url =
            asc_store::s3::effective_endpoint(cfg).map_err(|e| RouteError::Other(e.to_string()))?;
        Ok(self.guard.pin(&url, Shape::Endpoint).await?)
    }

    /// The store for a target: credentials, guard and client, in that order.
    pub async fn live(&self, t: &Target, sealed: &[u8]) -> Result<Live, RouteError> {
        let cfg = self.config(t, sealed)?;
        let pinned = self.pin(&cfg).await?;
        let connector = pinned
            .connector()
            .map_err(|_| RouteError::Other("cannot build the HTTP client".into()))?;
        let (store, base) = asc_store::s3::open_target(&cfg, connector)
            .map_err(|_| RouteError::Other("cannot build the storage client".into()))?;
        Ok(Live {
            id: t.id.clone(),
            version: t.version,
            store,
            base,
            addrs: pinned.addrs().to_vec(),
            checked: Mutex::new(Instant::now()),
        })
    }
}

// ------------------------------------------------------------------------------ verification

/// The steps of a verification, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Resolve,
    Connect,
    Put,
    Get,
    Delete,
}

/// The outcome of one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub step: Step,
    pub ok: bool,
    pub latency_ms: u32,
    /// A fixed, sanitised phrase.
    pub message: String,
}

/// The outcome of a verification.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.checks.len() == 5 && self.checks.iter().all(|c| c.ok)
    }

    /// The message of the first failing step, for `last_error`.
    pub fn failure(&self) -> Option<String> {
        self.checks
            .iter()
            .find(|c| !c.ok)
            .map(|c| format!("{}: {}", step_name(c.step), c.message))
    }

    fn push(&mut self, step: Step, started: Instant, result: Result<(), String>) -> bool {
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let ok = result.is_ok();
        self.checks.push(Check {
            step,
            ok,
            latency_ms,
            message: result.err().unwrap_or_else(|| "ok".into()),
        });
        ok
    }
}

pub fn step_name(s: Step) -> &'static str {
    match s {
        Step::Resolve => "resolve",
        Step::Connect => "connect",
        Step::Put => "put",
        Step::Get => "get",
        Step::Delete => "delete",
    }
}

fn reqwest_phrase(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "the connection timed out".into()
    } else if e.is_connect() {
        let text = format!("{e:?}").to_ascii_lowercase();
        if text.contains("certificate") || text.contains("tls") || text.contains("ssl") {
            "the TLS handshake failed: check the endpoint certificate".into()
        } else {
            "the endpoint refused the connection or is unreachable".into()
        }
    } else {
        "the endpoint did not answer as an HTTP server".into()
    }
}

/// Runs the handshake against a target: resolve and judge the endpoint, connect over TLS to the
/// pinned address, then write a random probe object under `<prefix>/.asc-probe/`, read it back,
/// compare, and delete it. Stops at the first failing step, except that a written probe object
/// is always deleted.
pub async fn probe(builder: &Builder, t: &Target, sealed: &[u8]) -> Result<Report, RouteError> {
    let cfg = builder.config(t, sealed)?;
    let mut report = Report::default();

    let started = Instant::now();
    let pinned = match builder.pin(&cfg).await {
        Ok(p) => {
            report.push(Step::Resolve, started, Ok(()));
            p
        }
        Err(RouteError::Blocked(r)) => {
            report.push(Step::Resolve, started, Err(r.message().into()));
            return Ok(report);
        }
        Err(e) => {
            report.push(Step::Resolve, started, Err(e.to_string()));
            return Ok(report);
        }
    };

    // Connect: any HTTP answer proves the TLS connection to the pinned address works; S3
    // endpoints answer `/` with an error status.
    let started = Instant::now();
    let client = pinned
        .client(crate::netguard::Profile::Api)
        .map_err(|_| RouteError::Other("cannot build the HTTP client".into()))?;
    let connect = client
        .head(pinned.endpoint().as_string())
        .send()
        .await
        .map(drop)
        .map_err(|e| reqwest_phrase(&e));
    if !report.push(Step::Connect, started, connect) {
        return Ok(report);
    }

    let connector = pinned
        .connector()
        .map_err(|_| RouteError::Other("cannot build the HTTP client".into()))?;
    let (store, base) = asc_store::s3::open_target(&cfg, connector)
        .map_err(|_| RouteError::Other("cannot build the storage client".into()))?;
    let mut rnd = [0u8; 24];
    getrandom::fill(&mut rnd).map_err(|_| RouteError::Other("no randomness".into()))?;
    let name: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let location = base.clone().join(PROBE_DIR).join(name);
    let body = bytes::Bytes::from(rnd.to_vec());

    let started = Instant::now();
    let put = store
        .put(&location, PutPayload::from_bytes(body.clone()))
        .await
        .map(drop)
        .map_err(|e| sanitize(&e).to_owned());
    let wrote = put.is_ok();
    if !report.push(Step::Put, started, put) {
        return Ok(report);
    }

    let started = Instant::now();
    let get = async {
        let got = store
            .get(&location)
            .await
            .map_err(|e| sanitize(&e).to_owned())?
            .bytes()
            .await
            .map_err(|e| sanitize(&e).to_owned())?;
        if got == body {
            Ok(())
        } else {
            Err("the object read back differs from the one written".to_owned())
        }
    }
    .await;
    let read_ok = report.push(Step::Get, started, get);

    let started = Instant::now();
    let del = store
        .delete(&location)
        .await
        .map_err(|e| sanitize(&e).to_owned());
    report.push(Step::Delete, started, del);
    let _ = (wrote, read_ok);
    Ok(report)
}

// ------------------------------------------------------------------------------ the router

/// What the registry builds a repository's `BucketObjects` over.
pub struct Routed {
    pub store: Arc<dyn ObjectStore>,
    /// The root `BucketObjects` uses (`ns/<hex>`).
    pub root: Path,
    /// Changes whenever the route does; the registry rebuilds its entry then.
    pub stamp: u64,
}

struct CachedRoute {
    signature: String,
    routed: Arc<Routed>,
}

/// Maps repositories to their stores.
pub struct Router {
    pub db: Db,
    pub builder: Builder,
    default_store: Arc<dyn ObjectStore>,
    /// The registry's root (`<root>` in `<root>/ns/<hex>`).
    default_root: Path,
    live: Mutex<HashMap<String, Arc<Live>>>,
    meta: Mutex<HashMap<String, (Target, Instant)>>,
    routes: RwLock<HashMap<[u8; 16], CachedRoute>>,
    stamps: AtomicU64,
    repin: Duration,
}

impl Router {
    pub fn new(
        db: Db,
        builder: Builder,
        default_store: Arc<dyn ObjectStore>,
        default_root: Path,
        repin: Duration,
    ) -> Self {
        Self {
            db,
            builder,
            default_store,
            default_root,
            live: Mutex::default(),
            meta: Mutex::default(),
            routes: RwLock::default(),
            stamps: AtomicU64::new(1),
            repin,
        }
    }

    fn default_member(&self, ns: Ns) -> Member {
        Member {
            store: self.default_store.clone(),
            root: self.default_root.clone().join("ns").join(ns.hex()),
        }
    }

    /// Forgets what is cached about a target (after an update, a state change or a delete).
    pub fn invalidate(&self, id: &str) {
        if let Ok(mut m) = self.meta.lock() {
            m.remove(id);
        }
        if let Ok(mut l) = self.live.lock() {
            l.remove(id);
        }
    }

    async fn target(&self, id: &str) -> Result<Target, RouteError> {
        if let Ok(m) = self.meta.lock()
            && let Some((t, at)) = m.get(id)
            && at.elapsed() < META_TTL
        {
            return Ok(t.clone());
        }
        let t = self.db.target(id).await?.ok_or(RouteError::Gone)?;
        if let Ok(mut m) = self.meta.lock() {
            m.insert(id.to_owned(), (t.clone(), Instant::now()));
        }
        Ok(t)
    }

    /// The store of an enabled target, built or reused.
    pub async fn live(&self, id: &str) -> Result<Arc<Live>, RouteError> {
        let t = self.target(id).await?;
        if t.state != State::Enabled {
            return Err(RouteError::NotEnabled(t.state.as_str()));
        }
        let cached = self
            .live
            .lock()
            .ok()
            .and_then(|l| l.get(id).cloned())
            .filter(|l| l.version == t.version);
        if let Some(live) = cached {
            if live.age() < self.repin {
                return Ok(live);
            }
            // Judge the name again. The same addresses keep the same store (and the caches built
            // on it); a different set, vetted afresh, gets a new one.
            let sealed = self.db.sealed(id).await?.ok_or(RouteError::Gone)?;
            let cfg = self.builder.config(&t, &sealed)?;
            let pinned = self.builder.pin(&cfg).await?;
            if pinned.addrs() == live.addrs.as_slice() {
                live.touch();
                return Ok(live);
            }
        }
        let sealed = self.db.sealed(id).await?.ok_or(RouteError::Gone)?;
        let live = Arc::new(self.builder.live(&t, &sealed).await?);
        if let Ok(mut l) = self.live.lock() {
            l.insert(id.to_owned(), live.clone());
        }
        Ok(live)
    }

    async fn member_sig(&self, target_id: &str, ns: Ns) -> Result<(Member, String), RouteError> {
        if target_id.is_empty() {
            return Ok((self.default_member(ns), "default".into()));
        }
        let live = self.live(target_id).await?;
        // Keyed on the build (the pointer), so a rebuilt store changes the route's signature.
        let sig = format!("{}@{}#{:p}", live.id, live.version, Arc::as_ptr(&live));
        Ok((live.member(ns), sig))
    }

    /// The store for repository `repo_id` in namespace `ns`, or `None` for the instance default.
    pub async fn route(&self, repo_id: i64, ns: Ns) -> Result<Option<Arc<Routed>>, RouteError> {
        let Some(row) = self.db.route(repo_id).await? else {
            if let Ok(mut r) = self.routes.write() {
                r.remove(ns.as_bytes());
            }
            return Ok(None);
        };
        let (primary, psig) = self.member_sig(&row.target_id, ns).await?;
        let mut sig = format!("v{}|{psig}", row.version);
        let mut reads = vec![primary.clone()];
        let mut writes = vec![primary];
        if let Some(f) = &row.fallback {
            let (m, s) = self.member_sig(f, ns).await?;
            sig.push_str(&format!("|f{s}"));
            reads.push(m);
        }
        if let Some(t) = &row.mirror_to {
            let (m, s) = self.member_sig(t, ns).await?;
            sig.push_str(&format!("|m{s}"));
            writes.push(m);
        }
        if let Ok(r) = self.routes.read()
            && let Some(c) = r.get(ns.as_bytes())
            && c.signature == sig
        {
            return Ok(Some(c.routed.clone()));
        }
        let virtual_root = Path::from("ns").join(ns.hex());
        let store = RoutedStore::new(virtual_root.clone(), writes, reads)
            .ok_or_else(|| RouteError::Other("empty route".into()))?;
        let routed = Arc::new(Routed {
            store: Arc::new(store),
            root: virtual_root,
            stamp: self.stamps.fetch_add(1, Ordering::Relaxed),
        });
        if let Ok(mut r) = self.routes.write() {
            r.insert(
                *ns.as_bytes(),
                CachedRoute {
                    signature: sig,
                    routed: routed.clone(),
                },
            );
        }
        Ok(Some(routed))
    }

    /// The place `target_id` keeps namespace `ns` in (`""`: the instance default). The target
    /// must be enabled.
    pub async fn member(&self, target_id: &str, ns: Ns) -> Result<Member, RouteError> {
        Ok(self.member_sig(target_id, ns).await?.0)
    }

    /// Drops the cached route of a namespace so the next lookup builds it again.
    pub fn forget_route(&self, ns: Ns) {
        if let Ok(mut r) = self.routes.write() {
            r.remove(ns.as_bytes());
        }
    }

    /// The store and root of a namespace already routed on this process (`None`: the default).
    pub fn namespace(&self, ns: Ns) -> Option<(Arc<dyn ObjectStore>, Path)> {
        let r = self.routes.read().ok()?;
        let c = r.get(ns.as_bytes())?;
        Some((c.routed.store.clone(), c.routed.root.clone()))
    }

    /// Loads every route from the database, so namespaces this process has not opened yet (a
    /// garbage collection, a drop) find their objects. Also removes routes of repositories that
    /// are gone for good. Returns how many routes could not be built.
    pub async fn refresh(&self) -> usize {
        let mut failed = 0;
        let table = match self.db.route_table().await {
            Ok(t) => t,
            Err(_) => return 1,
        };
        let mut live_ns = std::collections::HashSet::new();
        for (ns, row) in table {
            let Some(ns) = Ns::from_bytes(&ns) else {
                continue;
            };
            live_ns.insert(*ns.as_bytes());
            if self.route(row.repo_id, ns).await.is_err() {
                failed += 1;
            }
        }
        if let Ok(mut r) = self.routes.write() {
            r.retain(|k, _| live_ns.contains(k));
        }
        let _ = self.db.sweep_routes().await;
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_text_is_reduced_to_a_fixed_phrase() {
        let secret_url = "https://bucket.s3.example.com/key?X-Amz-Signature=SECRETSIG";
        for (text, phrase) in [
            ("status code: 403 Forbidden AccessDenied", "access denied"),
            ("SignatureDoesNotMatch", "rejected the credentials"),
            ("NoSuchBucket", "not found"),
            ("error trying to connect: tls handshake eof", "TLS"),
            ("operation timed out", "timed out"),
            ("503 SlowDown", "rate limiting"),
            ("dns error: failed to lookup", "could not be reached"),
            ("QuotaExceeded", "quota"),
            ("something odd", "unexpected response"),
        ] {
            let e = object_store::Error::Generic {
                store: "S3",
                source: format!("{text} for {secret_url}").into(),
            };
            let out = sanitize(&e);
            assert!(out.contains(phrase), "{text}: {out}");
            assert!(!out.contains("SECRETSIG") && !out.contains("example.com"));
        }
        let e = object_store::Error::Unauthenticated {
            path: "k".into(),
            source: secret_url.into(),
        };
        assert_eq!(sanitize(&e), "the provider rejected the credentials");
    }

    #[test]
    fn report_failure_names_the_first_failing_step() {
        let mut r = Report::default();
        let t = Instant::now();
        r.push(Step::Resolve, t, Ok(()));
        r.push(Step::Connect, t, Err("the connection timed out".into()));
        assert!(!r.ok());
        assert_eq!(r.failure().unwrap(), "connect: the connection timed out");
    }
}
