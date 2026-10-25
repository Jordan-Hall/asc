//! Code intelligence benchmarks (SPEC 23.9, PLAN I27): query scenarios against `rg`, `git grep`
//! (worktree and `<rev>`) and Zoekt, and the pure gate evaluation. `run` executes the commands.

pub mod run;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Differences under this many milliseconds do not count for "not slower" (as in O05).
pub const NOISE_FLOOR_MS: f64 = 5.0;
pub const HUGE_P50_MS: f64 = 50.0;
pub const HUGE_P99_MS: f64 = 300.0;
pub const MIN_SPEEDUP: f64 = 10.0;
pub const INCREMENTAL_MS: f64 = 2_000.0;
pub const INCREMENTAL_EMBED_MS: f64 = 10_000.0;
pub const INDEX_RATIO: f64 = 0.4;
pub const DAEMON_IDLE_MIB: f64 = 64.0;
pub const DAEMON_LOAD_MIB: f64 = 96.0;
pub const HYBRID_POINTS: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Workload {
    Small,
    Medium,
    Huge,
}

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Workload::Small => "small",
            Workload::Medium => "medium",
            Workload::Huge => "huge",
        }
    }
}

/// The tools a query runs on. `asc` is the subject, the others are baselines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Asc,
    RgWarm,
    RgCold,
    GitGrepWorktree,
    GitGrepRev,
    Zoekt,
}

impl Tool {
    pub const ALL: [Tool; 6] = [
        Tool::Asc,
        Tool::RgWarm,
        Tool::RgCold,
        Tool::GitGrepWorktree,
        Tool::GitGrepRev,
        Tool::Zoekt,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Tool::Asc => "asc",
            Tool::RgWarm => "rg-warm",
            Tool::RgCold => "rg-cold",
            Tool::GitGrepWorktree => "git-grep",
            Tool::GitGrepRev => "git-grep-rev",
            Tool::Zoekt => "zoekt",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryKind {
    Literal,
    Regex,
    Icase,
    Word,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub name: String,
    pub kind: QueryKind,
    pub pattern: String,
}

#[derive(Deserialize)]
struct QueryFile {
    #[serde(default)]
    query: Vec<Query>,
}

/// Parses `scenarios.toml`; names must be unique and patterns non-empty and not flag-like
/// (a pattern starting with `-` would be read as an option by the baselines).
pub fn parse_queries(text: &str) -> Result<Vec<Query>, String> {
    let f: QueryFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = std::collections::BTreeSet::new();
    for q in &f.query {
        if q.pattern.is_empty() || q.pattern.starts_with('-') {
            return Err(format!("query {:?}: empty or option-like pattern", q.name));
        }
        if q.name.is_empty() || !seen.insert(q.name.clone()) {
            return Err(format!("query name {:?} is empty or repeated", q.name));
        }
    }
    if f.query.is_empty() {
        return Err("no queries".into());
    }
    Ok(f.query)
}

/// `q`-quantile (nearest rank); `None` when empty.
pub fn quantile(ms: &[f64], q: f64) -> Option<f64> {
    if ms.is_empty() {
        return None;
    }
    let mut s = ms.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let rank = ((q * s.len() as f64).ceil() as usize).clamp(1, s.len());
    Some(s[rank - 1])
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub runs: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

pub fn stat(ms: &[f64]) -> Option<Stat> {
    Some(Stat {
        runs: ms.len(),
        p50_ms: quantile(ms, 0.5)?,
        p99_ms: quantile(ms, 0.99)?,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    Ok(Stat),
    Unsupported { reason: String },
    Error { reason: String },
}

/// One query on one workload, with every tool's outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub workload: Workload,
    pub query: String,
    pub tools: BTreeMap<Tool, Outcome>,
}

/// Numbers that the query runner cannot take itself (server-side or eval measurements), supplied as
/// JSON by `asc serve admin codeintel stats` and the I28 eval. Any field may be absent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Median ASC commit, grams and tags only.
    pub incremental_ms: Option<f64>,
    /// Median ASC commit with CPU embeddings.
    pub incremental_embed_ms: Option<f64>,
    pub index_bytes: Option<u64>,
    pub text_bytes: Option<u64>,
    pub daemon_idle_rss_mib: Option<f64>,
    pub daemon_load_rss_mib: Option<f64>,
    /// NDCG@10 in points (0 to 100).
    pub hybrid_ndcg10: Option<f64>,
    pub bm25_ndcg10: Option<f64>,
}

/// Gate violations; empty means pass. `allow_unsupported` downgrades "asc or a baseline did not run"
/// and "not measured" to a skip (local runs only); a tool that ran and failed always fails.
pub fn gate(rows: &[Row], metrics: &Metrics, allow_unsupported: bool) -> Vec<String> {
    let mut out = Vec::new();
    let skip = |out: &mut Vec<String>, msg: String| {
        if !allow_unsupported {
            out.push(msg);
        }
    };
    for r in rows {
        let key = format!("{}/{}", r.workload.name(), r.query);
        let asc = match r.tools.get(&Tool::Asc) {
            Some(Outcome::Ok(s)) => s,
            Some(Outcome::Error { reason }) => {
                out.push(format!("{key}: asc failed ({reason})"));
                continue;
            }
            Some(Outcome::Unsupported { reason }) => {
                skip(&mut out, format!("{key}: asc did not run ({reason})"));
                continue;
            }
            None => {
                skip(&mut out, format!("{key}: asc missing"));
                continue;
            }
        };
        if r.workload == Workload::Huge && (asc.p50_ms > HUGE_P50_MS || asc.p99_ms > HUGE_P99_MS) {
            out.push(format!(
                "{key}: asc p50/p99 {:.1}/{:.1} ms exceeds {HUGE_P50_MS}/{HUGE_P99_MS} ms",
                asc.p50_ms, asc.p99_ms
            ));
        }
        for t in [Tool::GitGrepRev, Tool::RgCold] {
            match r.tools.get(&t) {
                Some(Outcome::Ok(b)) => match r.workload {
                    Workload::Small => {
                        if asc.p50_ms > b.p50_ms + NOISE_FLOOR_MS {
                            out.push(format!(
                                "{key}: asc {:.1} ms is slower than {} {:.1} ms",
                                asc.p50_ms,
                                t.name(),
                                b.p50_ms
                            ));
                        }
                    }
                    _ => {
                        if asc.p50_ms * MIN_SPEEDUP > b.p50_ms {
                            out.push(format!(
                                "{key}: asc {:.1} ms is not {MIN_SPEEDUP}x faster than {} {:.1} ms",
                                asc.p50_ms,
                                t.name(),
                                b.p50_ms
                            ));
                        }
                    }
                },
                Some(Outcome::Error { reason }) => {
                    out.push(format!("{key}: {} failed ({reason})", t.name()))
                }
                _ => skip(&mut out, format!("{key}: {} did not run", t.name())),
            }
        }
    }
    let mut limit = |name: &str, v: Option<f64>, max: f64| match v {
        Some(v) if v > max => out.push(format!("{name}: {v:.2} exceeds {max}")),
        Some(_) => {}
        None => {
            if !allow_unsupported {
                out.push(format!("{name}: not measured"));
            }
        }
    };
    limit(
        "incremental index (grams, tags) ms",
        metrics.incremental_ms,
        INCREMENTAL_MS,
    );
    limit(
        "incremental index with CPU embeddings ms",
        metrics.incremental_embed_ms,
        INCREMENTAL_EMBED_MS,
    );
    limit(
        "daemon idle RSS MiB",
        metrics.daemon_idle_rss_mib,
        DAEMON_IDLE_MIB,
    );
    limit(
        "daemon serving RSS MiB",
        metrics.daemon_load_rss_mib,
        DAEMON_LOAD_MIB,
    );
    let ratio = match (metrics.index_bytes, metrics.text_bytes) {
        (Some(i), Some(t)) if t > 0 => Some(i as f64 / t as f64),
        _ => None,
    };
    limit("index bytes / indexed text bytes", ratio, INDEX_RATIO);
    match (metrics.hybrid_ndcg10, metrics.bm25_ndcg10) {
        (Some(h), Some(b)) => {
            if h < b + HYBRID_POINTS {
                out.push(format!(
                    "semantic: hybrid NDCG@10 {h:.1} is below BM25-only {b:.1} + {HYBRID_POINTS}; \
                     `search` with `mode = semantic` must default to BM25 (record it in docs/BENCHMARKS.md)"
                ));
            }
        }
        _ => {
            if !allow_unsupported {
                out.push("semantic: NDCG@10 not measured".into());
            }
        }
    }
    out
}

fn cell(o: Option<&Outcome>) -> String {
    match o {
        Some(Outcome::Ok(s)) => format!("{:.1} / {:.1}", s.p50_ms, s.p99_ms),
        Some(Outcome::Unsupported { .. }) | None => "-".into(),
        Some(Outcome::Error { .. }) => "error".into(),
    }
}

/// Markdown table (p50 / p99 ms per tool) for the terminal and `docs/BENCHMARKS.md`.
pub fn table(rows: &[Row]) -> String {
    let mut s = String::from("| workload | query |");
    for t in Tool::ALL {
        let _ = write!(s, " {} |", t.name());
    }
    s.push_str("\n|---|---|");
    for _ in Tool::ALL {
        s.push_str("---|");
    }
    s.push('\n');
    for r in rows {
        let _ = write!(s, "| {} | {} |", r.workload.name(), r.query);
        for t in Tool::ALL {
            let _ = write!(s, " {} |", cell(r.tools.get(&t)));
        }
        s.push('\n');
    }
    s
}

/// The `docs/BENCHMARKS.md` section for a release label.
pub fn release_markdown(label: &str, versions: &str, machine: &str, rows: &[Row]) -> String {
    format!(
        "## Code intelligence {label}\n\n- tools: {versions}\n- machine: {machine}\n- cells: p50 / p99 in ms\n- reproduce: `cargo run -p asc-bench-codeintel --release -- --suite codeintel-quick`\n\n{}",
        table(rows)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(p50: f64, p99: f64) -> Outcome {
        Outcome::Ok(Stat {
            runs: 10,
            p50_ms: p50,
            p99_ms: p99,
        })
    }
    fn row(w: Workload, asc: Outcome, grep: f64, rg: f64) -> Row {
        let mut tools = BTreeMap::new();
        tools.insert(Tool::Asc, asc);
        tools.insert(Tool::GitGrepRev, st(grep, grep));
        tools.insert(Tool::RgCold, st(rg, rg));
        Row {
            workload: w,
            query: "q".into(),
            tools,
        }
    }
    fn full() -> Metrics {
        Metrics {
            incremental_ms: Some(1500.0),
            incremental_embed_ms: Some(8000.0),
            index_bytes: Some(30),
            text_bytes: Some(100),
            daemon_idle_rss_mib: Some(60.0),
            daemon_load_rss_mib: Some(90.0),
            hybrid_ndcg10: Some(70.0),
            bm25_ndcg10: Some(60.0),
        }
    }

    #[test]
    fn passing_run_has_no_violations() {
        let rows = [row(Workload::Huge, st(20.0, 100.0), 900.0, 800.0)];
        assert!(gate(&rows, &full(), false).is_empty());
    }

    #[test]
    fn huge_latency_and_speedup_gates() {
        let slow = [row(Workload::Huge, st(60.0, 100.0), 900.0, 800.0)];
        assert_eq!(gate(&slow, &full(), false).len(), 1);
        let tail = [row(Workload::Huge, st(20.0, 400.0), 900.0, 800.0)];
        assert_eq!(gate(&tail, &full(), false).len(), 1);
        let close = [row(Workload::Medium, st(40.0, 50.0), 300.0, 800.0)];
        let v = gate(&close, &full(), false);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("git-grep-rev"));
    }

    #[test]
    fn small_only_needs_not_slower() {
        let ok = [row(Workload::Small, st(12.0, 12.0), 10.0, 10.0)];
        assert!(gate(&ok, &full(), false).is_empty());
        let bad = [row(Workload::Small, st(30.0, 30.0), 10.0, 10.0)];
        assert_eq!(gate(&bad, &full(), false).len(), 2);
    }

    #[test]
    fn unsupported_and_unmeasured_fail_unless_allowed() {
        let un = Outcome::Unsupported {
            reason: "no search".into(),
        };
        let rows = [row(Workload::Huge, un, 900.0, 800.0)];
        assert_eq!(gate(&rows, &full(), false).len(), 1);
        assert!(gate(&rows, &full(), true).is_empty());
        let v = gate(&[], &Metrics::default(), false);
        assert_eq!(v.len(), 6, "{v:?}");
        assert!(gate(&[], &Metrics::default(), true).is_empty());
    }

    #[test]
    fn metric_gates() {
        let mut m = full();
        m.incremental_ms = Some(2500.0);
        m.index_bytes = Some(41);
        m.daemon_idle_rss_mib = Some(70.0);
        m.daemon_load_rss_mib = Some(100.0);
        m.hybrid_ndcg10 = Some(64.0);
        assert_eq!(gate(&[], &m, false).len(), 5);
    }

    #[test]
    fn errors_fail_even_when_unsupported_allowed() {
        let e = Outcome::Error {
            reason: "boom".into(),
        };
        let rows = [row(Workload::Huge, e, 900.0, 800.0)];
        assert_eq!(gate(&rows, &full(), true).len(), 1);
    }

    #[test]
    fn query_file_validation() {
        let shipped = include_str!("../scenarios.toml");
        assert_eq!(parse_queries(shipped).unwrap().len(), 5);
        assert!(parse_queries("[[query]]\nname='a'\nkind='literal'\npattern='--files'").is_err());
        assert!(parse_queries("").is_err());
        let dup = "[[query]]\nname='a'\nkind='word'\npattern='x'\n[[query]]\nname='a'\nkind='word'\npattern='y'";
        assert!(parse_queries(dup).is_err());
    }

    #[test]
    fn quantiles_and_table() {
        assert_eq!(quantile(&[3.0, 1.0, 2.0, 4.0], 0.5), Some(2.0));
        assert_eq!(quantile(&[], 0.5), None);
        let rows = [row(Workload::Small, st(1.0, 2.0), 3.0, 4.0)];
        let t = table(&rows);
        assert!(t.contains("| small | q | 1.0 / 2.0 |"), "{t}");
    }
}
