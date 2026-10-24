//! Code-intelligence metrics (SPEC §16.1, PLAN I29): `asc_ci_index_lag_seconds`,
//! `asc_ci_jobs{kind,state}`, `asc_ci_query_seconds{rpc}`, `asc_ci_candidates_ratio` and
//! `asc_ci_coverage`, registered into the server catalogue ([`Metrics::with_registry`]).
//!
//! Label values are bounded: `rpc` is one of [`RPCS`] (anything else is `other`), and `kind`
//! and `state` come from the `ci_jobs` schema, never from a request.

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};

use super::queue::Depth;
use crate::http::{Labels, Metrics};

/// The `CodeIntel` RPC label values; any other name is recorded as `other`.
pub const RPCS: &[&str] = &[
    "Search",
    "FindSymbols",
    "Definition",
    "References",
    "SemanticSearch",
    "Hybrid",
];

type FloatGauge = Gauge<f64, AtomicU64>;

fn query_histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.001, 2.0, 16))
}

/// The code-intelligence metric handles.
#[derive(Clone)]
pub struct CiMetrics {
    index_lag: Gauge,
    jobs: Family<Labels, Gauge>,
    query: Family<Labels, Histogram, fn() -> Histogram>,
    candidates_ratio: FloatGauge,
    coverage: FloatGauge,
}

impl CiMetrics {
    /// Creates the handles and registers them in `metrics`.
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self {
            index_lag: Gauge::default(),
            jobs: Family::default(),
            query: Family::new_with_constructor(query_histogram as fn() -> Histogram),
            candidates_ratio: FloatGauge::default(),
            coverage: FloatGauge::default(),
        };
        metrics.with_registry(|r| {
            r.register(
                "asc_ci_index_lag_seconds",
                "Age of the oldest unindexed code-intelligence job",
                m.index_lag.clone(),
            );
            r.register(
                "asc_ci_jobs",
                "Code-intelligence jobs by kind and state",
                m.jobs.clone(),
            );
            r.register(
                "asc_ci_query_seconds",
                "Code-intelligence query latency by RPC",
                m.query.clone(),
            );
            r.register(
                "asc_ci_candidates_ratio",
                "Share of files in scope the index left as candidates",
                m.candidates_ratio.clone(),
            );
            r.register(
                "asc_ci_coverage",
                "Indexed share of the tree",
                m.coverage.clone(),
            );
        });
        m
    }

    /// Sets the index lag from the oldest pending job's creation time (`None`: nothing pending).
    pub fn set_index_lag(&self, now: i64, oldest_pending: Option<i64>) {
        self.index_lag.set(lag_secs(now, oldest_pending));
    }

    /// Replaces the job gauge with `depth`; kinds and states that vanished disappear.
    pub fn set_jobs(&self, depth: &Depth) {
        self.jobs.clear();
        for (kind, state, n) in depth {
            self.jobs
                .get_or_create(&labels(&[("kind", kind), ("state", state)]))
                .set(*n);
        }
    }

    /// Records one query's latency.
    pub fn observe_query(&self, rpc: &str, took: Duration) {
        self.query
            .get_or_create(&labels(&[("rpc", rpc_label(rpc))]))
            .observe(took.as_secs_f64());
    }

    /// Records `candidates` files left by the index out of `total` in scope.
    pub fn set_candidates(&self, candidates: u64, total: u64) {
        self.candidates_ratio.set(ratio(candidates, total));
    }

    /// Records `indexed` of `total` files in scope (an empty scope is fully covered).
    pub fn set_coverage(&self, indexed: u64, total: u64) {
        self.coverage
            .set(if total == 0 { 1.0 } else { ratio(indexed, total) });
    }
}

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Maps an RPC name to a bounded label value.
pub fn rpc_label(rpc: &str) -> &'static str {
    RPCS.iter().find(|r| **r == rpc).copied().unwrap_or("other")
}

/// Seconds since the oldest pending job; zero when none is pending or the clock stepped back.
pub fn lag_secs(now: i64, oldest_pending: Option<i64>) -> i64 {
    oldest_pending.map_or(0, |t| now.saturating_sub(t).max(0))
}

/// `num / den` clamped to `[0, 1]`; zero when `den` is zero.
pub fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        return 0.0;
    }
    (num as f64 / den as f64).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Metrics, CiMetrics) {
        let m = Metrics::new();
        let ci = CiMetrics::register(&m);
        (m, ci)
    }

    #[test]
    fn the_catalogue_names_are_exposed() {
        let (m, ci) = fixture();
        ci.set_index_lag(100, Some(40));
        ci.set_jobs(&vec![("file".into(), "queued".into(), 3)]);
        ci.observe_query("Search", Duration::from_millis(5));
        ci.set_candidates(1, 4);
        ci.set_coverage(3, 4);
        let out = m.render();
        for name in [
            "asc_ci_index_lag_seconds 60",
            "asc_ci_jobs{kind=\"file\",state=\"queued\"} 3",
            "asc_ci_query_seconds_count{rpc=\"Search\"} 1",
            "asc_ci_candidates_ratio 0.25",
            "asc_ci_coverage 0.75",
        ] {
            assert!(out.contains(name), "missing {name} in {out}");
        }
    }

    #[test]
    fn vanished_job_states_do_not_linger() {
        let (m, ci) = fixture();
        ci.set_jobs(&vec![("file".into(), "queued".into(), 3)]);
        ci.set_jobs(&vec![("file".into(), "dead".into(), 1)]);
        let out = m.render();
        assert!(!out.contains("state=\"queued\""), "{out}");
        assert!(out.contains("state=\"dead\"} 1"), "{out}");
    }

    #[test]
    fn rpc_labels_are_bounded() {
        assert_eq!(rpc_label("Search"), "Search");
        assert_eq!(rpc_label("attacker-chosen-12345"), "other");
        let (m, ci) = fixture();
        ci.observe_query("x-1", Duration::ZERO);
        ci.observe_query("x-2", Duration::ZERO);
        let out = m.render();
        assert!(out.contains("rpc=\"other\""));
        assert!(!out.contains("x-1"));
    }

    #[test]
    fn lag_and_ratios_never_go_out_of_range() {
        assert_eq!(lag_secs(10, None), 0);
        assert_eq!(lag_secs(10, Some(20)), 0);
        assert_eq!(lag_secs(i64::MIN, Some(i64::MAX)), 0);
        assert_eq!(ratio(1, 0), 0.0);
        assert_eq!(ratio(9, 3), 1.0);
        let (m, ci) = fixture();
        ci.set_coverage(0, 0);
        assert!(m.render().contains("asc_ci_coverage 1.0"));
    }
}
