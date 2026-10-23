//! Shared observability definitions (SPEC §16.1, §16.4): the stable log fields, the event
//! catalogue, the metric catalogue that `docs/metrics.md` mirrors, and the OTLP endpoint check.
//!
//! The tables are data only. A test keeps `docs/metrics.md` and [`METRICS`] equal, and checks
//! that every daemon metric is rendered by the daemon's own endpoint code.

#![allow(dead_code)]

/// Tiers that emit logs and metrics (the `tier` log field).
pub const TIERS: &[&str] = &["cli", "daemon", "warmer", "server", "cache", "forgejo"];

/// Stable JSON log fields, in order.
pub const LOG_FIELDS: &[&str] = &[
    "ts",
    "level",
    "tier",
    "event",
    "repo",
    "op_id",
    "rpc",
    "duration_ms",
    "outcome",
    "trace_id",
];

/// The `event` names. Adding one is a catalogue change; renaming one is a breaking change.
pub const EVENTS: &[&str] = &[
    "land.conflict",
    "land.retry",
    "outbox.retry",
    "outbox.dead",
    "cache.corrupt",
    "switch.pending",
    "reconcile.orphan",
    "import.mismatch",
    "rate.limited",
    "hydration.denied",
    "daemon.restart",
    "daemon.hung",
];

/// Whether `event` is in the catalogue.
pub fn is_event(event: &str) -> bool {
    EVENTS.contains(&event)
}

/// One catalogue entry.
pub struct Metric {
    pub name: &'static str,
    pub kind: &'static str,
    pub tiers: &'static str,
    pub labels: &'static str,
}

/// The names are fixed by SPEC §16.1.
pub const METRICS: &[Metric] = &[
    Metric {
        name: "asc_rpc_duration_seconds",
        kind: "histogram",
        tiers: "server,cache",
        labels: "rpc,code",
    },
    Metric {
        name: "asc_store_op_seconds",
        kind: "histogram",
        tiers: "server,cache",
        labels: "op,backend",
    },
    Metric {
        name: "asc_ref_cas_conflicts_total",
        kind: "counter",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_land_total",
        kind: "counter",
        tiers: "server,cache",
        labels: "outcome",
    },
    Metric {
        name: "asc_hook_seconds",
        kind: "histogram",
        tiers: "server,cache",
        labels: "hook,outcome",
    },
    Metric {
        name: "asc_outbox_lag_seconds",
        kind: "gauge",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_outbox_failures_total",
        kind: "counter",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_mirror_import_lag_seconds",
        kind: "gauge",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_derive_lag_changesets",
        kind: "gauge",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_changeset_reads",
        kind: "histogram",
        tiers: "server,cache",
        labels: "rpc",
    },
    Metric {
        name: "asc_graph_walk_nodes",
        kind: "histogram",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_cache_requests_total",
        kind: "counter",
        tiers: "server,cache,daemon",
        labels: "tier,result",
    },
    Metric {
        name: "asc_rate_limited_total",
        kind: "counter",
        tiers: "server,cache",
        labels: "class",
    },
    Metric {
        name: "asc_inflight_requests",
        kind: "gauge",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_shed_total",
        kind: "counter",
        tiers: "server,cache",
        labels: "reason",
    },
    Metric {
        name: "asc_coalesced_total",
        kind: "counter",
        tiers: "server,cache",
        labels: "",
    },
    Metric {
        name: "asc_turbo_hit_ratio",
        kind: "gauge",
        tiers: "server,cache",
        labels: "scope",
    },
    Metric {
        name: "asc_rss_bytes",
        kind: "gauge",
        tiers: "server,cache,daemon",
        labels: "",
    },
    Metric {
        name: "asc_daemon_hydrations_total",
        kind: "counter",
        tiers: "daemon",
        labels: "process",
    },
    Metric {
        name: "asc_daemon_hydration_seconds",
        kind: "histogram",
        tiers: "daemon",
        labels: "",
    },
    Metric {
        name: "asc_daemon_fetch_bytes_total",
        kind: "counter",
        tiers: "daemon",
        labels: "",
    },
    Metric {
        name: "asc_daemon_cache_bytes",
        kind: "gauge",
        tiers: "daemon",
        labels: "dir",
    },
    Metric {
        name: "asc_daemon_journal_entries",
        kind: "gauge",
        tiers: "daemon",
        labels: "",
    },
    Metric {
        name: "asc_daemon_pending_paths",
        kind: "gauge",
        tiers: "daemon",
        labels: "",
    },
    Metric {
        name: "asc_daemon_reconcile_paths_total",
        kind: "counter",
        tiers: "daemon",
        labels: "",
    },
    Metric {
        name: "asc_daemon_provider_errors_total",
        kind: "counter",
        tiers: "daemon",
        labels: "op",
    },
    Metric {
        name: "asc_daemon_uptime_seconds",
        kind: "gauge",
        tiers: "daemon",
        labels: "",
    },
    Metric {
        name: "asc_ci_index_lag_seconds",
        kind: "gauge",
        tiers: "server",
        labels: "",
    },
    Metric {
        name: "asc_ci_jobs",
        kind: "gauge",
        tiers: "server",
        labels: "kind,state",
    },
    Metric {
        name: "asc_ci_query_seconds",
        kind: "histogram",
        tiers: "server",
        labels: "rpc",
    },
    Metric {
        name: "asc_ci_candidates_ratio",
        kind: "gauge",
        tiers: "server",
        labels: "",
    },
    Metric {
        name: "asc_ci_coverage",
        kind: "gauge",
        tiers: "server",
        labels: "",
    },
    Metric {
        name: "asc_client_retries_total",
        kind: "counter",
        tiers: "cli,daemon",
        labels: "",
    },
    Metric {
        name: "asc_warm_lag_seconds",
        kind: "gauge",
        tiers: "warmer",
        labels: "",
    },
    Metric {
        name: "asc_warm_objects_total",
        kind: "counter",
        tiers: "warmer",
        labels: "",
    },
    Metric {
        name: "asc_warm_failures_total",
        kind: "counter",
        tiers: "warmer",
        labels: "",
    },
    Metric {
        name: "forgejo_asc_rpc_seconds",
        kind: "histogram",
        tiers: "forgejo",
        labels: "method",
    },
    Metric {
        name: "forgejo_asc_shadow_mismatch_total",
        kind: "counter",
        tiers: "forgejo",
        labels: "method",
    },
];

/// Longest accepted OTLP endpoint.
const MAX_ENDPOINT: usize = 2048;

/// Validates `ASC_OTLP_ENDPOINT`: HTTPS, or plain HTTP to a loopback host only. Returns the
/// endpoint unchanged; unset input is not an error and disables export.
pub fn check_otlp_endpoint(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if v.len() > MAX_ENDPOINT || v.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("ASC_OTLP_ENDPOINT is malformed".into());
    }
    if v.starts_with("https://") && v.len() > "https://".len() {
        return Ok(Some(v.to_owned()));
    }
    if let Some(rest) = v.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let host = if let Some(h) = host.strip_prefix('[') {
            h.split(']').next().unwrap_or("")
        } else {
            host.rsplit_once(':').map_or(host, |(h, _)| h)
        };
        if matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return Ok(Some(v.to_owned()));
        }
        return Err("ASC_OTLP_ENDPOINT: plain http is allowed only to loopback".into());
    }
    Err("ASC_OTLP_ENDPOINT must be an https:// URL".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const DOC: &str = include_str!("../../../docs/metrics.md");
    const DAEMON_SRC: &str = include_str!("daemon/metrics.rs");
    const ALERTS: &str = include_str!("../../../deploy/alerts.yml");

    fn doc_names() -> Vec<String> {
        DOC.lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|l| l.split('`').next())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn doc_and_catalogue_agree() {
        let doc: BTreeSet<String> = doc_names().into_iter().collect();
        let cat: BTreeSet<String> = METRICS.iter().map(|m| m.name.to_owned()).collect();
        assert_eq!(doc, cat);
        assert_eq!(doc_names().len(), METRICS.len(), "duplicate row");
    }

    #[test]
    fn names_are_well_formed() {
        for m in METRICS {
            assert!(m.name.starts_with("asc_") || m.name.starts_with("forgejo_asc_"));
            assert!(m.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
            assert!(["counter", "gauge", "histogram"].contains(&m.kind));
            if m.kind == "counter" {
                assert!(m.name.ends_with("_total"), "{}", m.name);
            }
        }
    }

    #[test]
    fn daemon_metrics_are_registered_by_the_binary() {
        // Not yet emitted by the daemon; remove from this list when the metric lands.
        const PENDING: &[&str] = &["asc_daemon_hydration_seconds"];
        for m in METRICS
            .iter()
            .filter(|m| m.tiers == "daemon" && !PENDING.contains(&m.name))
        {
            assert!(
                DAEMON_SRC.contains(m.name),
                "{} not in daemon/metrics.rs",
                m.name
            );
        }
    }

    #[test]
    fn alerts_use_only_catalogued_metrics() {
        for tok in ALERTS.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if tok.starts_with("asc_") || tok.starts_with("forgejo_asc_") {
                let base = tok
                    .trim_end_matches("_bucket")
                    .trim_end_matches("_sum")
                    .trim_end_matches("_count");
                assert!(
                    METRICS.iter().any(|m| m.name == tok || m.name == base),
                    "{tok} not catalogued"
                );
            }
        }
        assert!(ALERTS.contains("AscCacheHitRateBelowSloThreeDays"));
    }

    #[test]
    fn events_and_fields() {
        assert_eq!(EVENTS.len(), 12);
        assert!(is_event("land.conflict") && !is_event("land.other"));
        assert_eq!(LOG_FIELDS.len(), 10);
        assert_eq!(TIERS.len(), 6);
    }

    #[test]
    fn otlp_endpoint_rules() {
        assert_eq!(check_otlp_endpoint(None), Ok(None));
        assert!(
            check_otlp_endpoint(Some("https://otel.example:4318"))
                .unwrap()
                .is_some()
        );
        assert!(
            check_otlp_endpoint(Some("http://127.0.0.1:4318"))
                .unwrap()
                .is_some()
        );
        assert!(
            check_otlp_endpoint(Some("http://[::1]:4318/v1"))
                .unwrap()
                .is_some()
        );
        assert!(check_otlp_endpoint(Some("http://otel.example")).is_err());
        assert!(check_otlp_endpoint(Some("http://localhost.evil.com")).is_err());
        assert!(check_otlp_endpoint(Some("ftp://x")).is_err());
        assert!(check_otlp_endpoint(Some("https://a b")).is_err());
    }
}
