//! Request, queue, workflow, agent-run and storage metrics (docs/metrics.md, "Server scrape").
//!
//! They join the catalogue [`Metrics`] serves on the admin listener's `/metrics` (loopback, or a
//! bearer token off loopback). Requests are counted by a layer on the public router. The rest are
//! read from the database when `/metrics` is scraped, so every instance reports what the whole
//! deployment holds and no code path of the features has to remember to count.
//!
//! Label values are bounded: `kind` and `class` are fixed words, `state`, `queue` come from the
//! schemas, and an `rpc` name seen after [`MAX_RPC_LABELS`] distinct ones is `other`.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use sqlx::{AnyPool, Row};

use crate::http::{Labels, Metrics};

/// Most distinct `rpc` label values kept before the rest are `other`.
pub const MAX_RPC_LABELS: usize = 512;

/// `AgentRunState` names by number (docs/BUILDOUT.md, agent runs).
const AGENT_STATES: [(i64, &str); 8] = [
    (1, "queued"),
    (2, "running"),
    (3, "waiting_input"),
    (4, "succeeded"),
    (5, "failed"),
    (6, "cancelled"),
    (7, "budget_exceeded"),
    (8, "expired"),
];
/// `WorkflowRunState` names by number.
const WORKFLOW_STATES: [(i64, &str); 6] = [
    (1, "queued"),
    (2, "running"),
    (3, "waiting_approval"),
    (4, "succeeded"),
    (5, "failed"),
    (6, "cancelled"),
];
/// The states that are still going; the others are finished and counted as totals.
const ACTIVE: [&str; 4] = ["queued", "running", "waiting_input", "waiting_approval"];

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// The handles of these metrics.
pub struct Stats {
    metrics: Arc<Metrics>,
    requests: Family<Labels, Counter>,
    agent_active: Family<Labels, Gauge>,
    agent_total: Family<Labels, Counter>,
    workflow_active: Family<Labels, Gauge>,
    workflow_total: Family<Labels, Counter>,
    storage_targets: Family<Labels, Gauge>,
    storage_migrations: Family<Labels, Gauge>,
    queue_depth: Family<Labels, Gauge>,
    rpcs: Mutex<HashSet<String>>,
}

impl Stats {
    /// Creates the handles and registers them in `metrics`.
    pub fn register(metrics: &Arc<Metrics>) -> Arc<Self> {
        let s = Arc::new(Self {
            metrics: metrics.clone(),
            requests: Family::default(),
            agent_active: Family::default(),
            agent_total: Family::default(),
            workflow_active: Family::default(),
            workflow_total: Family::default(),
            storage_targets: Family::default(),
            storage_migrations: Family::default(),
            queue_depth: Family::default(),
            rpcs: Mutex::default(),
        });
        metrics.with_registry(|r| {
            r.register(
                "asc_requests",
                "Requests served on the public listener by kind and status class",
                s.requests.clone(),
            );
            r.register(
                "asc_queue_depth",
                "Work waiting in a queue (outbox, agent_runs, workflow_runs)",
                s.queue_depth.clone(),
            );
            r.register(
                "asc_workflow_runs_active",
                "Workflow runs that are not finished, by state",
                s.workflow_active.clone(),
            );
            r.register(
                "asc_workflow_runs",
                "Finished workflow runs by state",
                s.workflow_total.clone(),
            );
            r.register(
                "asc_agent_runs_active",
                "Agent runs that are not finished, by state",
                s.agent_active.clone(),
            );
            r.register(
                "asc_agent_runs",
                "Finished agent runs by state",
                s.agent_total.clone(),
            );
            r.register(
                "asc_storage_targets",
                "Bring-your-own storage targets by state",
                s.storage_targets.clone(),
            );
            r.register(
                "asc_storage_migrations",
                "Storage migrations by state",
                s.storage_migrations.clone(),
            );
        });
        s
    }

    /// Counts one finished request (and times it when it was an RPC).
    pub fn observe(
        &self,
        path: &str,
        content_type: &str,
        status: u16,
        grpc_status: Option<&str>,
        took: f64,
    ) {
        let grpc = content_type.starts_with("application/grpc");
        let kind = if grpc {
            "grpc"
        } else if path == "/mcp" {
            "mcp"
        } else if path.starts_with("/ui") {
            "ui"
        } else if path.ends_with("/info/refs")
            || path.ends_with("/git-upload-pack")
            || path.ends_with("/git-receive-pack")
        {
            "git"
        } else {
            "http"
        };
        let failed_rpc = grpc_status.is_some_and(|g| g != "0");
        let class = match status {
            _ if failed_rpc => "error",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };
        self.requests
            .get_or_create(&labels(&[("kind", kind), ("class", class)]))
            .inc();
        if grpc {
            let rpc = self.rpc_label(path);
            let code = match grpc_status {
                Some("0") | None if status == 200 => "OK".to_owned(),
                Some(g) => format!("grpc-{g}"),
                None => format!("http-{status}"),
            };
            self.metrics
                .rpc_duration
                .get_or_create(&labels(&[("rpc", &rpc), ("code", &code)]))
                .observe(took);
        }
    }

    /// The `rpc` label of a gRPC path (`/asc.v1.Repo/GetObjects` -> `asc.v1.Repo/GetObjects`).
    fn rpc_label(&self, path: &str) -> String {
        let name = path.trim_start_matches('/');
        let shaped = name.len() <= 128
            && name.split('/').count() == 2
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/'));
        if !shaped {
            return "other".into();
        }
        let mut seen = self.rpcs.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.contains(name) || (seen.len() < MAX_RPC_LABELS && seen.insert(name.to_owned())) {
            name.to_owned()
        } else {
            "other".into()
        }
    }

    /// Reads the database and sets the queue, run and storage metrics. A table that cannot be
    /// read keeps its last values and is logged.
    pub async fn refresh(&self, pool: &AnyPool) {
        let mut queued = [("outbox", 0_i64), ("agent_runs", 0), ("workflow_runs", 0)];
        match count_by(
            pool,
            "SELECT CAST(COUNT(*) AS BIGINT) AS n, 0 AS k FROM outbox WHERE done_at IS NULL",
        )
        .await
        {
            Ok(rows) => queued[0].1 = rows.first().map_or(0, |r| r.1),
            Err(e) => tracing::warn!(event = "metrics.outbox", error = %e),
        }
        match count_by(pool, "SELECT CAST(COUNT(*) AS BIGINT) AS n, CAST(state AS BIGINT) AS k FROM agent_runs GROUP BY state").await {
            Ok(rows) => {
                queued[1].1 = rows.iter().find(|r| r.0 == 1).map_or(0, |r| r.1);
                set_states(&self.agent_active, &self.agent_total, &AGENT_STATES, &rows);
            }
            Err(e) => tracing::warn!(event = "metrics.agent_runs", error = %e),
        }
        match count_by(pool, "SELECT CAST(COUNT(*) AS BIGINT) AS n, CAST(state AS BIGINT) AS k FROM workflow_runs GROUP BY state").await {
            Ok(rows) => {
                queued[2].1 = rows.iter().find(|r| r.0 == 1).map_or(0, |r| r.1);
                set_states(&self.workflow_active, &self.workflow_total, &WORKFLOW_STATES, &rows);
            }
            Err(e) => tracing::warn!(event = "metrics.workflow_runs", error = %e),
        }
        for (queue, n) in queued {
            self.queue_depth
                .get_or_create(&labels(&[("queue", queue)]))
                .set(n);
        }
        for (family, sql) in [
            (
                &self.storage_targets,
                "SELECT state, CAST(COUNT(*) AS BIGINT) AS n FROM storage_targets GROUP BY state",
            ),
            (
                &self.storage_migrations,
                "SELECT state, CAST(COUNT(*) AS BIGINT) AS n FROM storage_migrations GROUP BY state",
            ),
        ] {
            match sqlx::query(sql).fetch_all(pool).await {
                Ok(rows) => {
                    family.clear();
                    for r in rows {
                        let (state, n): (String, i64) = (
                            r.try_get("state").unwrap_or_default(),
                            r.try_get("n").unwrap_or(0),
                        );
                        // States are words of the schema; anything else is not a label.
                        if state.len() <= 32
                            && state.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                        {
                            family.get_or_create(&labels(&[("state", &state)])).set(n);
                        }
                    }
                }
                Err(e) => tracing::warn!(event = "metrics.storage", error = %e),
            }
        }
    }
}

/// `(state number, count)` rows of a grouped query; the first column is the count.
async fn count_by(pool: &AnyPool, sql: &str) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| Ok((r.try_get::<i64, _>("k")?, r.try_get::<i64, _>("n")?)))
        .collect()
}

/// Active states become gauges; finished ones become totals (a state with no rows reads 0).
fn set_states(
    active: &Family<Labels, Gauge>,
    total: &Family<Labels, Counter>,
    names: &[(i64, &str)],
    rows: &[(i64, i64)],
) {
    for (number, name) in names {
        let n = rows.iter().find(|r| r.0 == *number).map_or(0, |r| r.1);
        let l = labels(&[("state", name)]);
        if ACTIVE.contains(name) {
            active.get_or_create(&l).set(n);
        } else {
            total
                .get_or_create(&l)
                .inner()
                .store(u64::try_from(n).unwrap_or(0), Ordering::Relaxed);
        }
    }
}

/// The layer function: counts every request the public router serves.
pub async fn observe(State(stats): State<Arc<Stats>>, req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let started = Instant::now();
    let res = next.run(req).await;
    let grpc_status = res
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    stats.observe(
        &path,
        &content_type,
        res.status().as_u16(),
        grpc_status.as_deref(),
        started.elapsed().as_secs_f64(),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(m: &Metrics) -> String {
        m.render()
    }

    #[test]
    fn requests_are_counted_by_kind_and_class_and_rpcs_are_timed() {
        let m = Arc::new(Metrics::new());
        let s = Stats::register(&m);
        s.observe("/healthz", "", 200, None, 0.001);
        s.observe("/mcp", "application/json", 401, None, 0.001);
        s.observe(
            "/asc.v1.Repo/GetObjects",
            "application/grpc",
            200,
            Some("0"),
            0.01,
        );
        s.observe(
            "/asc.v1.Repo/GetObjects",
            "application/grpc",
            200,
            Some("7"),
            0.01,
        );
        s.observe("/o/r.git/info/refs", "", 200, None, 0.01);
        let t = text(&m);
        assert!(
            t.contains(r#"asc_requests_total{kind="http",class="2xx"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_requests_total{kind="mcp",class="4xx"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_requests_total{kind="grpc",class="2xx"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_requests_total{kind="grpc",class="error"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_requests_total{kind="git",class="2xx"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(
                r#"asc_rpc_duration_seconds_count{rpc="asc.v1.Repo/GetObjects",code="OK"} 1"#
            ),
            "{t}"
        );
        assert!(t.contains(r#"code="grpc-7"} 1"#), "{t}");
    }

    #[tokio::test]
    async fn the_layer_counts_what_the_router_serves() {
        use tower::ServiceExt as _;
        let m = Arc::new(Metrics::new());
        let s = Stats::register(&m);
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(s, observe));
        for path in ["/healthz", "/healthz", "/nothing"] {
            let _ = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
        }
        let t = m.render();
        assert!(
            t.contains(r#"asc_requests_total{kind="http",class="2xx"} 2"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_requests_total{kind="http",class="4xx"} 1"#),
            "{t}"
        );
    }

    #[test]
    fn rpc_labels_are_bounded_and_shaped() {
        let m = Arc::new(Metrics::new());
        let s = Stats::register(&m);
        assert_eq!(s.rpc_label("/not a path"), "other");
        assert_eq!(s.rpc_label("/a/b/c"), "other");
        assert_eq!(s.rpc_label(&format!("/{}/m", "a".repeat(200))), "other");
        for i in 0..MAX_RPC_LABELS {
            assert_ne!(s.rpc_label(&format!("/p.S/M{i}")), "other");
        }
        assert_eq!(s.rpc_label("/p.S/Late"), "other", "the set is full");
        assert_eq!(
            s.rpc_label("/p.S/M0"),
            "p.S/M0",
            "a known name still reports"
        );
    }

    #[tokio::test]
    async fn the_database_metrics_follow_the_tables() {
        let pool = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
        crate::migrate::migrate(&pool).await.unwrap();
        let runs = crate::agents::runs::RunStore::new(pool.clone());
        for state in [1_i64, 1, 2, 4, 4, 4, 5] {
            let id = runs
                .create(&crate::agents::runs::NewRun {
                    repo: "o/r".into(),
                    agent: "a".into(),
                    title: String::new(),
                    target: "master".into(),
                    base: Vec::new(),
                    task: "t".into(),
                    paths: vec![],
                    ai_key_id: "k".into(),
                    model: "m".into(),
                    limits: crate::agents::tokens::Budget::default(),
                    created_by: "u".into(),
                    starter: "u".into(),
                    owner_id: 1,
                    token_id: String::new(),
                    workflow_run_id: 0,
                    workflow_step: String::new(),
                })
                .await
                .unwrap();
            sqlx::query("UPDATE agent_runs SET state = $1 WHERE id = $2")
                .bind(state)
                .bind(i64::try_from(id).unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES (1, X'01', X'00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let m = Arc::new(Metrics::new());
        let s = Stats::register(&m);
        s.refresh(&pool).await;
        let t = text(&m);
        assert!(
            t.contains(r#"asc_agent_runs_active{state="queued"} 2"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_agent_runs_active{state="running"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_agent_runs_total{state="succeeded"} 3"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_agent_runs_total{state="failed"} 1"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_queue_depth{queue="agent_runs"} 2"#),
            "{t}"
        );
        assert!(t.contains(r#"asc_queue_depth{queue="outbox"} 1"#), "{t}");
        assert!(
            t.contains(r#"asc_queue_depth{queue="workflow_runs"} 0"#),
            "{t}"
        );
        assert!(
            t.contains(r#"asc_workflow_runs_active{state="waiting_approval"} 0"#),
            "{t}"
        );
    }
}
