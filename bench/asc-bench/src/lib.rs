//! Benchmark gate, git vs asc (SPEC 16.4, PLAN O05).
//!
//! This file holds the pure parts (statistics, the gate, the baseline check and
//! the renderers) so they are unit tested without spawning anything. `synth` (`bench/gen`) is
//! the deterministic repository generator, `run` executes scenarios.

pub mod cloud;
pub mod run;
pub mod synth;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// An asc run may be at most this much slower than git where the Scion model does not apply.
pub const ELSEWHERE_SLACK: f64 = 1.10;
/// An asc run may be at most this much slower than the stored baseline.
pub const REGRESSION_SLACK: f64 = 1.20;
/// Differences under this many milliseconds are noise for the percentage gates.
pub const NOISE_FLOOR_MS: f64 = 5.0;

/// Whether the Scion-model gate (asc strictly faster) or the 10% gate applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Scion,
    Elsewhere,
}

/// Wall-clock summary of one tool on one scenario, in milliseconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub runs: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

/// Outcome of one tool on one scenario.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    Ok(Stat),
    Unsupported { reason: String },
    Error { reason: String },
}

/// Bytes on disk after a scenario that creates a checkout (clone, worktree add).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub git: Option<u64>,
    pub asc: Option<u64>,
}

/// One row of the table: a scenario on a workload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub workload: String,
    pub scenario: String,
    pub class: Class,
    pub git: Outcome,
    pub asc: Outcome,
    #[serde(default)]
    pub disk: Disk,
}

impl Row {
    pub fn key(&self) -> String {
        format!("{}/{}", self.workload, self.scenario)
    }
}

/// `q`-quantile (nearest rank) of `ms`; `None` when empty.
pub fn quantile(ms: &[f64], q: f64) -> Option<f64> {
    if ms.is_empty() {
        return None;
    }
    let mut s = ms.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let rank = ((q * s.len() as f64).ceil() as usize).clamp(1, s.len());
    Some(s[rank - 1])
}

pub fn stat(ms: &[f64]) -> Option<Stat> {
    Some(Stat {
        runs: ms.len(),
        p50_ms: quantile(ms, 0.5)?,
        p95_ms: quantile(ms, 0.95)?,
    })
}

/// `bench/baseline.json`: asc p50 per `workload/scenario`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, f64>,
}

impl Baseline {
    pub fn from_rows(rows: &[Row]) -> Self {
        let entries = rows
            .iter()
            .filter_map(|r| match &r.asc {
                Outcome::Ok(s) => Some((r.key(), s.p50_ms)),
                _ => None,
            })
            .collect();
        Baseline {
            version: 1,
            entries,
        }
    }
}

/// Gate violations for the rows. Empty means the gate passes.
/// `allow_unsupported` downgrades "asc cannot run it" from a failure to a skip (local runs only).
pub fn gate(rows: &[Row], baseline: &Baseline, allow_unsupported: bool) -> Vec<String> {
    let mut out = Vec::new();
    for r in rows {
        let key = r.key();
        let g = match &r.git {
            Outcome::Ok(s) => s,
            other => {
                out.push(format!("{key}: git did not run ({})", describe(other)));
                continue;
            }
        };
        let c = match &r.asc {
            Outcome::Ok(s) => s,
            other => {
                let unsupported = matches!(other, Outcome::Unsupported { .. });
                if !(unsupported && allow_unsupported) {
                    out.push(format!("{key}: asc did not run ({})", describe(other)));
                }
                continue;
            }
        };
        match r.class {
            Class::Scion => {
                if c.p50_ms >= g.p50_ms {
                    out.push(format!(
                        "{key}: asc {:.1} ms is not faster than git {:.1} ms (Scion-model scenario)",
                        c.p50_ms, g.p50_ms
                    ));
                }
            }
            Class::Elsewhere => {
                if c.p50_ms > g.p50_ms * ELSEWHERE_SLACK && c.p50_ms - g.p50_ms > NOISE_FLOOR_MS {
                    out.push(format!(
                        "{key}: asc {:.1} ms is more than 10% slower than git {:.1} ms",
                        c.p50_ms, g.p50_ms
                    ));
                }
            }
        }
        if let Some(&b) = baseline.entries.get(&key)
            && c.p50_ms > b * REGRESSION_SLACK
            && c.p50_ms - b > NOISE_FLOOR_MS
        {
            out.push(format!(
                "{key}: asc {:.1} ms regressed more than 20% against the baseline {:.1} ms",
                c.p50_ms, b
            ));
        }
    }
    out
}

fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Ok(_) => "ok".into(),
        Outcome::Unsupported { reason } => format!("unsupported: {reason}"),
        Outcome::Error { reason } => format!("error: {reason}"),
    }
}

fn cell(o: &Outcome) -> String {
    match o {
        Outcome::Ok(s) => format!("{:.1} / {:.1}", s.p50_ms, s.p95_ms),
        Outcome::Unsupported { .. } => "unsupported".into(),
        Outcome::Error { .. } => "error".into(),
    }
}

fn ratio(r: &Row) -> String {
    match (&r.git, &r.asc) {
        (Outcome::Ok(g), Outcome::Ok(c)) if g.p50_ms > 0.0 => {
            format!("{:.2}x", c.p50_ms / g.p50_ms)
        }
        _ => "-".into(),
    }
}

/// Markdown table (also printed to the terminal): wall p50 / p95 in ms per tool and the asc/git ratio.
pub fn table(rows: &[Row]) -> String {
    let mut s = String::from(
        "| workload | scenario | gate | git p50 / p95 ms | asc p50 / p95 ms | asc/git |\n|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let class = if r.class == Class::Scion {
            "faster"
        } else {
            "within 10%"
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} |",
            r.workload,
            r.scenario,
            class,
            cell(&r.git),
            cell(&r.asc),
            ratio(r)
        );
    }
    s
}

/// The `docs/BENCHMARKS.md` section for one release.
pub fn release_markdown(
    release: &str,
    git_version: &str,
    asc_version: &str,
    machine: &str,
    rows: &[Row],
) -> String {
    let mut s = format!(
        "## {release}\n\n- git: `{git_version}`\n- asc: `{asc_version}`\n- machine: {machine}\n"
    );
    s.push_str("- reproduce: `cargo run -p asc-bench --release -- --suite quick`\n\n");
    s.push_str(&table(rows));
    let disk: Vec<_> = rows
        .iter()
        .filter(|r| r.disk.git.is_some() || r.disk.asc.is_some())
        .collect();
    if !disk.is_empty() {
        s.push_str("\nBytes on disk after the scenario:\n\n| workload | scenario | git | asc |\n|---|---|---|---|\n");
        for r in disk {
            let f = |v: Option<u64>| v.map_or("-".into(), |b| b.to_string());
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} |",
                r.workload,
                r.scenario,
                f(r.disk.git),
                f(r.disk.asc)
            );
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(p50: f64) -> Outcome {
        Outcome::Ok(Stat {
            runs: 5,
            p50_ms: p50,
            p95_ms: p50,
        })
    }
    fn row(class: Class, git: Outcome, asc: Outcome) -> Row {
        Row {
            workload: "small".into(),
            scenario: "s".into(),
            class,
            git,
            asc,
            disk: Disk::default(),
        }
    }

    #[test]
    fn quantiles() {
        assert_eq!(quantile(&[], 0.5), None);
        assert_eq!(quantile(&[5.0, 1.0, 3.0], 0.5), Some(3.0));
        assert_eq!(quantile(&[1.0, 2.0, 3.0, 4.0], 0.95), Some(4.0));
        assert_eq!(quantile(&[7.0], 0.95), Some(7.0));
    }

    #[test]
    fn scion_requires_strictly_faster() {
        let b = Baseline::default();
        assert!(gate(&[row(Class::Scion, ok(100.0), ok(90.0))], &b, false).is_empty());
        assert_eq!(
            gate(&[row(Class::Scion, ok(100.0), ok(100.0))], &b, false).len(),
            1
        );
        assert_eq!(
            gate(&[row(Class::Scion, ok(100.0), ok(101.0))], &b, false).len(),
            1
        );
    }

    #[test]
    fn elsewhere_allows_ten_percent() {
        let b = Baseline::default();
        assert!(gate(&[row(Class::Elsewhere, ok(100.0), ok(110.0))], &b, false).is_empty());
        assert_eq!(
            gate(&[row(Class::Elsewhere, ok(100.0), ok(111.0))], &b, false).len(),
            1
        );
        // below the noise floor a large ratio is not a failure
        assert!(gate(&[row(Class::Elsewhere, ok(1.0), ok(3.0))], &b, false).is_empty());
    }

    #[test]
    fn baseline_regression_over_twenty_percent_fails() {
        let mut b = Baseline::default();
        b.entries.insert("small/s".into(), 50.0);
        assert!(gate(&[row(Class::Elsewhere, ok(100.0), ok(60.0))], &b, false).is_empty());
        let v = gate(&[row(Class::Scion, ok(100.0), ok(61.0))], &b, false);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("baseline"));
    }

    #[test]
    fn unsupported_asc_fails_unless_allowed() {
        let u = Outcome::Unsupported {
            reason: "not supported yet".into(),
        };
        let rows = [row(Class::Scion, ok(10.0), u)];
        assert_eq!(gate(&rows, &Baseline::default(), false).len(), 1);
        assert!(gate(&rows, &Baseline::default(), true).is_empty());
    }

    #[test]
    fn git_failure_and_asc_error_always_fail() {
        let e = Outcome::Error {
            reason: "boom".into(),
        };
        assert_eq!(
            gate(
                &[row(Class::Scion, e.clone(), ok(1.0))],
                &Baseline::default(),
                true
            )
            .len(),
            1
        );
        assert_eq!(
            gate(&[row(Class::Scion, ok(1.0), e)], &Baseline::default(), true).len(),
            1
        );
    }

    #[test]
    fn baseline_roundtrip_keeps_only_ok_rows() {
        let rows = [row(Class::Scion, ok(10.0), ok(5.0))];
        let b = Baseline::from_rows(&rows);
        assert_eq!(b.entries.get("small/s"), Some(&5.0));
        let j = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<Baseline>(&j).unwrap(), b);
    }

    #[test]
    fn table_renders_ratio() {
        let t = table(&[row(Class::Scion, ok(100.0), ok(50.0))]);
        assert!(t.contains("0.50x"), "{t}");
    }
}
