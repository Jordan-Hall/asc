//! The server's plain HTTP routes (SPEC §8.1, §16.1; PLAN U43c). The public [`router`] serves
//! `/healthz`, a status-only `/readyz` and `/.well-known/asc`. The [`admin_router`], bound to
//! `[admin] listen`, serves `/metrics` and the detailed `/readyz`.
//!
//! `/readyz` fails when the database does not answer, when the oldest undelivered outbox row is
//! older than [`MAX_OUTBOX_LAG_SECS`], or when a registered [`Probe`] (the store, Forgejo) fails.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use serde::Serialize;
use sqlx::AnyPool;

use crate::outbox::Outbox;
use crate::registry::Extensions;

/// `/readyz` fails when the outbox lags by more than this (SPEC §16.1).
pub const MAX_OUTBOX_LAG_SECS: i64 = 300;
/// Longest a single readiness check may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The most `objects` and `seeds` URLs advertised.
pub const MAX_ADVERTISED: usize = 16;
const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// A label set such as `[("rpc", "GetObjects"), ("code", "OK")]`.
pub type Labels = Vec<(String, String)>;

fn histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.001, 2.0, 16))
}

fn count_histogram() -> Histogram {
    Histogram::new(exponential_buckets(1.0, 4.0, 12))
}

/// The server metric catalogue (SPEC §16.1). Names are fixed; units add the label sets they need.
pub struct Metrics {
    registry: Mutex<Registry>,
    pub rpc_duration: Family<Labels, Histogram, fn() -> Histogram>,
    pub store_op: Family<Labels, Histogram, fn() -> Histogram>,
    pub ref_cas_conflicts: Counter,
    pub land: Family<Labels, Counter>,
    pub hook: Family<Labels, Histogram, fn() -> Histogram>,
    pub outbox_lag: Gauge,
    pub outbox_failures: Counter,
    pub mirror_import_lag: Gauge,
    pub derive_lag: Gauge,
    pub changeset_reads: Family<Labels, Histogram, fn() -> Histogram>,
    pub graph_walk_nodes: Histogram,
    pub cache_requests: Family<Labels, Counter>,
    pub rate_limited: Family<Labels, Counter>,
    pub inflight: Gauge,
    pub shed: Family<Labels, Counter>,
    pub coalesced: Counter,
    pub turbo_hit_ratio: Family<Labels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    pub rss_bytes: Gauge,
}

impl Metrics {
    pub fn new() -> Self {
        let m = Self {
            registry: Mutex::new(Registry::default()),
            rpc_duration: Family::new_with_constructor(histogram as fn() -> Histogram),
            store_op: Family::new_with_constructor(histogram as fn() -> Histogram),
            ref_cas_conflicts: Counter::default(),
            land: Family::default(),
            hook: Family::new_with_constructor(histogram as fn() -> Histogram),
            outbox_lag: Gauge::default(),
            outbox_failures: Counter::default(),
            mirror_import_lag: Gauge::default(),
            derive_lag: Gauge::default(),
            changeset_reads: Family::new_with_constructor(count_histogram as fn() -> Histogram),
            graph_walk_nodes: count_histogram(),
            cache_requests: Family::default(),
            rate_limited: Family::default(),
            inflight: Gauge::default(),
            shed: Family::default(),
            coalesced: Counter::default(),
            turbo_hit_ratio: Family::default(),
            rss_bytes: Gauge::default(),
        };
        {
            let mut r = m.registry.lock().unwrap_or_else(PoisonError::into_inner);
            r.register(
                "asc_rpc_duration_seconds",
                "RPC latency",
                m.rpc_duration.clone(),
            );
            r.register(
                "asc_store_op_seconds",
                "Object store latency",
                m.store_op.clone(),
            );
            r.register(
                "asc_ref_cas_conflicts",
                "Ref compare-and-swap conflicts",
                m.ref_cas_conflicts.clone(),
            );
            r.register("asc_land", "Land attempts by outcome", m.land.clone());
            r.register("asc_hook_seconds", "Hook latency", m.hook.clone());
            r.register(
                "asc_outbox_lag_seconds",
                "Age of the oldest undelivered outbox row",
                m.outbox_lag.clone(),
            );
            r.register(
                "asc_outbox_failures",
                "Failed outbox deliveries",
                m.outbox_failures.clone(),
            );
            r.register(
                "asc_mirror_import_lag_seconds",
                "Mirror import lag",
                m.mirror_import_lag.clone(),
            );
            r.register(
                "asc_derive_lag_changesets",
                "Changesets awaiting derivation",
                m.derive_lag.clone(),
            );
            r.register(
                "asc_changeset_reads",
                "Changesets read per RPC",
                m.changeset_reads.clone(),
            );
            r.register(
                "asc_graph_walk_nodes",
                "Graph nodes visited per walk",
                m.graph_walk_nodes.clone(),
            );
            r.register(
                "asc_cache_requests",
                "Cache lookups by tier and result",
                m.cache_requests.clone(),
            );
            r.register(
                "asc_rate_limited",
                "Requests refused by a rate limit",
                m.rate_limited.clone(),
            );
            r.register(
                "asc_inflight_requests",
                "Requests in flight",
                m.inflight.clone(),
            );
            r.register("asc_shed", "Requests shed under load", m.shed.clone());
            r.register("asc_coalesced", "Coalesced requests", m.coalesced.clone());
            r.register(
                "asc_turbo_hit_ratio",
                "Turbo cache hit ratio by scope",
                m.turbo_hit_ratio.clone(),
            );
            r.register("asc_rss_bytes", "Resident set size", m.rss_bytes.clone());
        }
        m
    }

    /// Registers a metric a later unit adds to the catalogue.
    pub fn with_registry<R>(&self, f: impl FnOnce(&mut Registry) -> R) -> R {
        f(&mut self.registry.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The OpenMetrics text exposition.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let r = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        // Writing to a `String` cannot fail.
        let _ = encode(&mut out, &r);
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// A dependency `/readyz` checks (the store, Forgejo).
#[async_trait]
pub trait Probe: Send + Sync {
    fn name(&self) -> &'static str;
    async fn check(&self) -> Result<(), String>;
}

/// What `/.well-known/asc` advertises (SPEC §6.3).
#[derive(Clone, Debug, Default, Serialize)]
pub struct WellKnown {
    /// The public URL of this server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Object-tier URLs, tried first for `HasObjects`/`GetObjects`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<String>,
    /// The seed-snapshot base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeds: Option<String>,
}

impl WellKnown {
    /// Drops entries beyond [`MAX_ADVERTISED`].
    pub fn bounded(mut self) -> Self {
        self.objects.truncate(MAX_ADVERTISED);
        self
    }
}

/// Shared state of the HTTP routes.
#[derive(Clone)]
pub struct HttpState {
    pool: AnyPool,
    outbox: Arc<Outbox>,
    metrics: Arc<Metrics>,
    well_known: Arc<WellKnown>,
    probes: Arc<Mutex<Vec<Arc<dyn Probe>>>>,
}

impl HttpState {
    pub fn new(
        pool: AnyPool,
        outbox: Arc<Outbox>,
        metrics: Arc<Metrics>,
        well_known: WellKnown,
    ) -> Self {
        Self {
            pool,
            outbox,
            metrics,
            well_known: Arc::new(well_known.bounded()),
            probes: Arc::default(),
        }
    }

    /// Adds a dependency check to `/readyz`.
    pub fn add_probe(&self, p: Arc<dyn Probe>) {
        self.probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(p);
    }

    /// Every failed check, as `name: reason`; empty when the server is ready.
    pub async fn readiness(&self) -> Vec<String> {
        let mut failed = Vec::new();
        let db = tokio::time::timeout(PROBE_TIMEOUT, sqlx::query("SELECT 1").execute(&self.pool));
        match db.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => failed.push(format!("database: {e}")),
            Err(_) => failed.push("database: timed out".into()),
        }
        match self.outbox.lag_secs().await {
            Ok(lag) if lag > MAX_OUTBOX_LAG_SECS => {
                failed.push(format!("outbox: lag {lag}s exceeds {MAX_OUTBOX_LAG_SECS}s"));
            }
            Ok(_) => {}
            Err(e) => failed.push(format!("outbox: {e}")),
        }
        let probes: Vec<_> = self
            .probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for p in probes {
            match tokio::time::timeout(PROBE_TIMEOUT, p.check()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed.push(format!("{}: {e}", p.name())),
                Err(_) => failed.push(format!("{}: timed out", p.name())),
            }
        }
        failed
    }
}

/// Liveness: a body-less 200.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Public readiness: a status code only. The reasons name internal dependencies, so they go to
/// the log and to the admin listener, never to this caller.
async fn readyz(State(s): State<HttpState>) -> StatusCode {
    let failed = s.readiness().await;
    if failed.is_empty() {
        StatusCode::OK
    } else {
        tracing::warn!(event = "readyz.failed", reasons = ?failed);
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Admin readiness: the status code plus the JSON list of failed checks.
async fn readyz_detailed(State(s): State<HttpState>) -> Response {
    let failed = s.readiness().await;
    let code = if failed.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        axum::Json(serde_json::json!({ "ready": failed.is_empty(), "failed": failed })),
    )
        .into_response()
}

/// Compares two byte strings without an early exit on the first differing byte.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// Requires `Authorization: Bearer <token>` when the admin listener has a token.
async fn admin_auth(
    State(token): State<Option<Arc<str>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(expected) = token {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !ct_eq(given.as_bytes(), expected.as_bytes()) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
            )
                .into_response();
        }
    }
    next.run(req).await
}

async fn metrics(State(s): State<HttpState>) -> Response {
    match s.outbox.lag_secs().await {
        Ok(lag) => {
            s.metrics.outbox_lag.set(lag);
        }
        Err(e) => tracing::warn!(event = "metrics.outbox_lag", error = %e),
    }
    ([(header::CONTENT_TYPE, OPENMETRICS)], s.metrics.render()).into_response()
}

async fn well_known(State(s): State<HttpState>) -> Response {
    (
        [(header::CACHE_CONTROL, "public, max-age=300")],
        axum::Json((*s.well_known).clone()),
    )
        .into_response()
}

/// The public routes for `state`: no metrics, no readiness details.
pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/.well-known/asc", get(well_known))
        .with_state(state)
}

/// The admin routes (`/metrics`, detailed `/readyz`). With a `token`, every route requires it.
pub fn admin_router(state: HttpState, token: Option<Arc<str>>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/readyz", get(readyz_detailed))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(token, admin_auth))
}

/// One registration line in `lib.rs`: mounts the routes on `ext`.
pub fn register(ext: &Extensions, state: HttpState) {
    ext.http.register(router(state));
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::*;
    use crate::migrate;
    use crate::outbox::OutboxConfig;

    async fn state(well_known: WellKnown) -> (HttpState, Arc<Outbox>, AnyPool) {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let metrics = Arc::new(Metrics::new());
        let outbox = Arc::new(Outbox::new(
            pool.clone(),
            metrics.clone(),
            OutboxConfig::default(),
        ));
        let st = HttpState::new(pool.clone(), outbox.clone(), metrics, well_known);
        (st, outbox, pool)
    }

    async fn send(app: Router, path: &str, auth: Option<&str>) -> (u16, String) {
        let mut req = axum::http::Request::get(path);
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let res = app
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status().as_u16();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn get_path(st: &HttpState, path: &str) -> (u16, String) {
        send(router(st.clone()), path, None).await
    }

    async fn admin_get(st: &HttpState, path: &str) -> (u16, String) {
        send(admin_router(st.clone(), None), path, None).await
    }

    #[tokio::test]
    async fn health_and_ready() {
        let (st, _, _) = state(WellKnown::default()).await;
        assert_eq!(get_path(&st, "/healthz").await, (200, String::new()));
        assert_eq!(get_path(&st, "/readyz").await, (200, String::new()));
    }

    #[tokio::test]
    async fn public_listener_does_not_serve_metrics() {
        let (st, _, _) = state(WellKnown::default()).await;
        assert_eq!(get_path(&st, "/metrics").await.0, 404);
    }

    #[tokio::test]
    async fn admin_serves_metrics_and_detailed_readiness_on_loopback() {
        let (st, _, _) = state(WellKnown::default()).await;
        let (code, body) = admin_get(&st, "/metrics").await;
        assert_eq!(code, 200);
        assert!(body.contains("asc_rss_bytes"));
        let (code, body) = admin_get(&st, "/readyz").await;
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ready"], true);
        assert_eq!(admin_get(&st, "/healthz").await.0, 404);
    }

    #[tokio::test]
    async fn admin_token_is_required_and_compared_exactly() {
        let (st, _, _) = state(WellKnown::default()).await;
        let app = || admin_router(st.clone(), Some(Arc::from("s3cret")));
        for path in ["/metrics", "/readyz"] {
            assert_eq!(send(app(), path, None).await.0, 401);
            assert_eq!(send(app(), path, Some("Bearer wrong")).await.0, 401);
            assert_eq!(send(app(), path, Some("Bearer s3cre")).await.0, 401);
            assert_eq!(send(app(), path, Some("Bearer s3cret!")).await.0, 401);
            assert_eq!(send(app(), path, Some("s3cret")).await.0, 401);
            assert_eq!(send(app(), path, Some("Bearer s3cret")).await.0, 200);
        }
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
    }

    #[tokio::test]
    async fn readyz_fails_when_outbox_lag_exceeds_five_minutes() {
        let (st, _, pool) = state(WellKnown::default()).await;
        let now = crate::outbox::now();
        let insert = |at: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES (1, x'01', x'7b7d', $1)",
                )
                .bind(at)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        insert(now - 60).await;
        assert_eq!(get_path(&st, "/readyz").await.0, 200);
        insert(now - MAX_OUTBOX_LAG_SECS - 10).await;
        let (code, body) = get_path(&st, "/readyz").await;
        assert_eq!(code, 503);
        assert!(body.is_empty(), "reasons stay out of the public response");
        let (code, body) = admin_get(&st, "/readyz").await;
        assert_eq!(code, 503);
        assert!(body.contains("outbox"));
    }

    #[tokio::test]
    async fn readyz_fails_when_a_probe_fails() {
        struct Down;
        #[async_trait]
        impl Probe for Down {
            fn name(&self) -> &'static str {
                "forgejo"
            }
            async fn check(&self) -> Result<(), String> {
                Err("refused".into())
            }
        }
        let (st, _, _) = state(WellKnown::default()).await;
        st.add_probe(Arc::new(Down));
        assert_eq!(st.readiness().await, vec!["forgejo: refused".to_string()]);
        assert_eq!(get_path(&st, "/readyz").await.0, 503);
    }

    #[tokio::test]
    async fn metrics_use_the_catalogue_names() {
        let (st, _, _) = state(WellKnown::default()).await;
        st.metrics.outbox_failures.inc();
        st.metrics
            .rpc_duration
            .get_or_create(&vec![
                ("rpc".into(), "GetObjects".into()),
                ("code".into(), "OK".into()),
            ])
            .observe(0.01);
        let (code, body) = admin_get(&st, "/metrics").await;
        assert_eq!(code, 200);
        for name in [
            "asc_outbox_lag_seconds",
            "asc_outbox_failures_total 1",
            "asc_rpc_duration_seconds_count{rpc=\"GetObjects\",code=\"OK\"} 1",
            "asc_rss_bytes",
            "asc_inflight_requests",
        ] {
            assert!(body.contains(name), "missing {name} in:\n{body}");
        }
    }

    #[tokio::test]
    async fn well_known_lists_objects_and_seeds_only_when_configured() {
        let (st, _, _) = state(WellKnown {
            server: Some("https://asc.example:443".into()),
            ..WellKnown::default()
        })
        .await;
        let (code, body) = get_path(&st, "/.well-known/asc").await;
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["server"], "https://asc.example:443");
        assert!(v.get("objects").is_none() && v.get("seeds").is_none());

        let (st, _, _) = state(WellKnown {
            server: Some("https://asc.example:443".into()),
            objects: (0..40).map(|i| format!("https://c{i}.example")).collect(),
            seeds: Some("https://seeds.example".into()),
        })
        .await;
        let v: serde_json::Value =
            serde_json::from_str(&get_path(&st, "/.well-known/asc").await.1).unwrap();
        assert_eq!(v["objects"].as_array().unwrap().len(), MAX_ADVERTISED);
        assert_eq!(v["seeds"], "https://seeds.example");
    }
}
