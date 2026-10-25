//! Commit-cloud scenarios (SPEC 21.3.6, PLAN B25), declared in `bench/scenarios/branchless.toml`.
//!
//! Three scenarios have no git equivalent, so they are checked against budgets instead of git:
//! commit latency with the cloud on against off, time from commit to visible on the server, and push
//! of an already uploaded stack. The two that need a server read `ASC_BENCH_REMOTE`; it must be an
//! https or ssh URL without a password. Without it they are reported as unsupported.

use crate::run::{Fail, Tool};
use crate::synth::{self, Config};
use crate::{Baseline, NOISE_FLOOR_MS, Outcome, REGRESSION_SLACK, Stat, stat};
use serde::Deserialize;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

const SCENARIOS: &str = include_str!("../../scenarios/branchless.toml");
const MAX_STDOUT: u64 = 256 * 1024;
const POLL: Duration = Duration::from_millis(50);
const CMD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    CommitLatency,
    TimeToVisible,
    PushUploaded,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// commit_latency: allowed slowdown of cloud on over off, in percent.
    pub noise_pct: Option<f64>,
    pub floor_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
    pub kind: Kind,
    pub runs: usize,
    /// Changed files per commit.
    pub files: u64,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    #[serde(default)]
    pub budget: Budget,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    scenario: Vec<Spec>,
}

/// Parse and validate a scenario file.
pub fn parse(text: &str) -> Result<Vec<Spec>, String> {
    let f: File = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = std::collections::BTreeSet::new();
    for s in &f.scenario {
        if !s.name.starts_with("branchless-") || s.name.len() > 64 {
            return Err(format!(
                "scenario name {:?} must start with branchless-",
                s.name
            ));
        }
        if !seen.insert(s.name.clone()) {
            return Err(format!("duplicate scenario {}", s.name));
        }
        if s.runs == 0 || s.runs > 200 || s.files == 0 || s.files > 10 {
            return Err(format!("{}: runs must be 1..=200 and files 1..=10", s.name));
        }
        if s.timeout_s == 0 || s.timeout_s > 120 {
            return Err(format!("{}: timeout_s must be 1..=120", s.name));
        }
        let b = &s.budget;
        let ok = match s.kind {
            Kind::CommitLatency => b.noise_pct.is_some_and(|p| (0.0..=100.0).contains(&p)),
            Kind::TimeToVisible => b.p50_ms.is_some() && b.p95_ms.is_some(),
            Kind::PushUploaded => true,
        };
        if !ok {
            return Err(format!("{}: budget does not fit kind {:?}", s.name, s.kind));
        }
    }
    Ok(f.scenario)
}

/// The built-in scenarios.
pub fn scenarios() -> Vec<Spec> {
    parse(SCENARIOS).expect("bench/scenarios/branchless.toml is valid")
}

/// Accept only https and ssh remotes without a password; the URL goes on a command line.
pub fn check_remote(url: &str) -> Result<(), String> {
    if url.is_empty() || url.len() > 2048 || url.chars().any(|c| c.is_control() || c == ' ') {
        return Err("remote URL is empty, too long or has control characters".into());
    }
    if url.starts_with('-') {
        return Err("remote URL must not start with '-'".into());
    }
    let lower = url.to_ascii_lowercase();
    if let Some((scheme, rest)) = lower.split_once("://") {
        if scheme != "https" && scheme != "ssh" {
            return Err(format!(
                "remote scheme {scheme:?} is not allowed (https, ssh)"
            ));
        }
        let auth = rest.split('/').next().unwrap_or("");
        if auth.split_once('@').is_some_and(|(u, _)| u.contains(':')) {
            return Err("remote URL must not carry a password".into());
        }
        return Ok(());
    }
    // scp-like `user@host:path`; a `::` transport (`ext::`) is not.
    if lower.contains("::") || !lower.contains('@') || !lower.contains(':') {
        return Err("remote must be an https:// or ssh:// URL or user@host:path".into());
    }
    Ok(())
}

/// Violations of a commit-latency budget: cloud on against cloud off.
pub fn check_commit(name: &str, off: &Stat, on: &Stat, b: &Budget) -> Vec<String> {
    let pct = b.noise_pct.unwrap_or(10.0);
    let floor = b.floor_ms.unwrap_or(NOISE_FLOOR_MS);
    let limit = off.p50_ms * (1.0 + pct / 100.0);
    if on.p50_ms > limit && on.p50_ms - off.p50_ms > floor {
        vec![format!(
            "{name}: commit with upload p50 {:.1} ms is over cloud off {:.1} ms by more than {pct}%",
            on.p50_ms, off.p50_ms
        )]
    } else {
        Vec::new()
    }
}

/// Violations of a time-to-visible budget.
pub fn check_visible(name: &str, s: &Stat, b: &Budget) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(l) = b.p50_ms
        && s.p50_ms >= l
    {
        v.push(format!(
            "{name}: p50 {:.0} ms is not under {l:.0} ms",
            s.p50_ms
        ));
    }
    if let Some(l) = b.p95_ms
        && s.p95_ms >= l
    {
        v.push(format!(
            "{name}: p95 {:.0} ms is not under {l:.0} ms",
            s.p95_ms
        ));
    }
    v
}

/// Violations against `bench/baseline.json` (key `cloud/<name>`).
pub fn check_baseline(name: &str, s: &Stat, base: &Baseline) -> Vec<String> {
    match base.entries.get(&format!("cloud/{name}")) {
        Some(&b) if s.p50_ms > b * REGRESSION_SLACK && s.p50_ms - b > NOISE_FLOOR_MS => {
            vec![format!(
                "{name}: p50 {:.1} ms is over the baseline {b:.1} ms by more than 20%",
                s.p50_ms
            )]
        }
        _ => Vec::new(),
    }
}

/// One scenario result.
#[derive(Clone, Debug)]
pub struct CloudRow {
    pub scenario: String,
    pub outcome: Outcome,
    /// Cloud-off comparison p50 for commit_latency.
    pub reference_p50: Option<f64>,
    pub violations: Vec<String>,
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}

fn clip(s: &str) -> String {
    s.trim().chars().take(300).collect()
}

/// Run with captured, bounded stdout and a timeout.
fn capture(
    tool: &Tool,
    cwd: &Path,
    a: &[String],
    limit: Duration,
) -> Result<(Duration, String), Fail> {
    let mut cmd = tool.command(cwd, a);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| Fail::Error(format!("spawn: {e}")))?;
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = (&mut out).take(MAX_STDOUT).read_to_string(&mut s);
        let _ = std::io::copy(&mut out, &mut std::io::sink());
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = (&mut err).take(8 * 1024).read_to_string(&mut s);
        let _ = std::io::copy(&mut err, &mut std::io::sink());
        s
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Fail::Error(format!("{} timed out", a.join(" "))));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(Fail::Error(e.to_string())),
        }
    };
    let elapsed = start.elapsed();
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    if status.success() {
        Ok((elapsed, stdout))
    } else if crate::run::classify(&stderr) {
        Err(Fail::Unsupported(clip(&stderr)))
    } else {
        Err(Fail::Error(format!(
            "{} failed ({status}): {}",
            a.join(" "),
            clip(&stderr)
        )))
    }
}

fn io(e: std::io::Error) -> Fail {
    Fail::Error(e.to_string())
}

fn make_commit(tool: &Tool, repo: &Path, cfg: &Config, n: u64) -> Result<Duration, Fail> {
    synth::churn(cfg, repo, n).map_err(io)?;
    tool.run(repo, &args(&["add", "-A"]))?;
    tool.run(repo, &args(&["commit", "-q", "-m", &format!("bench {n}")]))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn synced_version(tool: &Tool, repo: &Path) -> Result<Option<u64>, Fail> {
    let (_, out) = capture(
        tool,
        repo,
        &args(&["cloud", "status", "--json"]),
        CMD_TIMEOUT,
    )?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| Fail::Error(format!("cloud status: {e}")))?;
    Ok(v.get("synced_version").and_then(|x| x.as_u64()))
}

/// Wait until the synced workspace version passes `after`.
fn wait_visible(
    tool: &Tool,
    repo: &Path,
    after: Option<u64>,
    limit: Duration,
) -> Result<Option<u64>, Fail> {
    let start = Instant::now();
    loop {
        let v = synced_version(tool, repo)?;
        if v.is_some_and(|v| after.is_none_or(|a| v > a)) {
            return Ok(v);
        }
        if start.elapsed() > limit {
            return Err(Fail::Error(format!(
                "commit was not visible on the server within {} s (is the daemon running?)",
                limit.as_secs()
            )));
        }
        std::thread::sleep(POLL);
    }
}

fn setup(
    tool: &Tool,
    dir: &Path,
    cfg: &Config,
    cloud: &str,
    remote: Option<&str>,
) -> Result<(), Fail> {
    crate::run::build_repo(tool, dir, cfg)?;
    tool.run(dir, &args(&["config", "asc.cloud", cloud]))?;
    if let Some(url) = remote {
        tool.run(dir, &args(&["remote", "add", "origin", url]))?;
    }
    Ok(())
}

fn timed(runs: usize, mut one: impl FnMut(u64) -> Result<Duration, Fail>) -> Result<Stat, Fail> {
    let mut v = Vec::new();
    for i in 0..=runs as u64 {
        let d = one(i)?;
        if i > 0 {
            v.push(ms(d)); // run 0 is a warm-up
        }
    }
    stat(&v).ok_or_else(|| Fail::Error("no timed runs".into()))
}

fn run_one(
    asc: &Tool,
    sp: &Spec,
    cfg: &Config,
    work: &Path,
    remote: Option<&str>,
) -> Result<(Stat, Option<f64>), Fail> {
    let limit = Duration::from_secs(sp.timeout_s);
    let need_remote =
        || remote.ok_or_else(|| Fail::Unsupported("ASC_BENCH_REMOTE is not set".into()));
    match sp.kind {
        Kind::CommitLatency => {
            let mut res = Vec::new();
            for mode in ["off", "on"] {
                let dir = work.join(format!("{}-{mode}", sp.name));
                setup(asc, &dir, cfg, mode, None)?;
                res.push(timed(sp.runs, |i| make_commit(asc, &dir, cfg, 3000 + i))?);
            }
            let off = res[0].p50_ms;
            Ok((res.remove(1), Some(off)))
        }
        Kind::TimeToVisible => {
            let url = need_remote()?;
            let dir = work.join(&sp.name);
            setup(asc, &dir, cfg, "on", Some(url))?;
            let mut last = wait_visible(asc, &dir, None, limit)?;
            let s = timed(sp.runs, |i| {
                let start = Instant::now();
                make_commit(asc, &dir, cfg, 4000 + i)?;
                last = wait_visible(asc, &dir, last, limit)?;
                Ok(start.elapsed())
            })?;
            Ok((s, None))
        }
        Kind::PushUploaded => {
            let url = need_remote()?;
            let dir = work.join(&sp.name);
            setup(asc, &dir, cfg, "on", Some(url))?;
            let mut last = wait_visible(asc, &dir, None, limit)?;
            let tag = std::process::id();
            let s = timed(sp.runs, |i| {
                make_commit(asc, &dir, cfg, 5000 + i)?;
                last = wait_visible(asc, &dir, last, limit)?;
                let r = format!("HEAD:refs/heads/bench/{tag}-{i}");
                capture(asc, &dir, &args(&["push", "origin", &r]), limit).map(|(d, _)| d)
            })?;
            Ok((s, None))
        }
    }
}

/// Run the scenarios whose name contains `only` (all when `None`) under `work`.
pub fn run_cloud(
    asc: Option<&Tool>,
    only: Option<&str>,
    remote: Option<&str>,
    baseline: &Baseline,
    work: &Path,
) -> Vec<CloudRow> {
    let remote = match remote.map(|r| check_remote(r).map(|()| r)) {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    };
    let mut rows = Vec::new();
    for sp in scenarios()
        .into_iter()
        .filter(|s| only.is_none_or(|o| s.name.contains(o)))
    {
        eprintln!("cloud/{} ...", sp.name);
        let mut cfg = Config::small();
        cfg.churn_files = sp.files;
        let res = match (&remote, asc) {
            (Err(e), _) => Err(Fail::Error(format!("ASC_BENCH_REMOTE: {e}"))),
            (_, None) => Err(Fail::Unsupported(
                "asc binary not given (set ASC_BIN or --asc)".into(),
            )),
            (Ok(r), Some(c)) => run_one(c, &sp, &cfg, work, *r),
        };
        let mut row = CloudRow {
            scenario: sp.name.clone(),
            outcome: Outcome::Error {
                reason: String::new(),
            },
            reference_p50: None,
            violations: Vec::new(),
        };
        match res {
            Ok((s, reference)) => {
                row.violations = match (sp.kind, reference) {
                    (Kind::CommitLatency, Some(off)) => {
                        let off = Stat {
                            runs: s.runs,
                            p50_ms: off,
                            p95_ms: off,
                        };
                        check_commit(&sp.name, &off, &s, &sp.budget)
                    }
                    (Kind::TimeToVisible, _) => check_visible(&sp.name, &s, &sp.budget),
                    _ => Vec::new(),
                };
                row.violations
                    .extend(check_baseline(&sp.name, &s, baseline));
                row.reference_p50 = reference;
                row.outcome = Outcome::Ok(s);
            }
            Err(Fail::Unsupported(reason)) => row.outcome = Outcome::Unsupported { reason },
            Err(Fail::Error(reason)) => row.outcome = Outcome::Error { reason },
        }
        rows.push(row);
    }
    rows
}

/// Gate violations of the cloud rows; unsupported rows fail unless `allow_unsupported`.
pub fn gate_cloud(rows: &[CloudRow], allow_unsupported: bool) -> Vec<String> {
    let mut out = Vec::new();
    for r in rows {
        match &r.outcome {
            Outcome::Ok(_) => out.extend(r.violations.iter().cloned()),
            Outcome::Unsupported { reason } if !allow_unsupported => {
                out.push(format!(
                    "cloud/{}: asc did not run (unsupported: {reason})",
                    r.scenario
                ));
            }
            Outcome::Unsupported { .. } => {}
            Outcome::Error { reason } => out.push(format!(
                "cloud/{}: asc did not run (error: {reason})",
                r.scenario
            )),
        }
    }
    out
}

/// `cloud/<name>` baseline entries from the Ok rows.
pub fn baseline_entries(rows: &[CloudRow]) -> Vec<(String, f64)> {
    rows.iter()
        .filter_map(|r| match &r.outcome {
            Outcome::Ok(s) => Some((format!("cloud/{}", r.scenario), s.p50_ms)),
            _ => None,
        })
        .collect()
}

/// Table text for the cloud rows.
pub fn table(rows: &[CloudRow]) -> String {
    let mut t = String::from("\nbranchless (cloud) scenarios\n");
    for r in rows {
        let cell = match &r.outcome {
            Outcome::Ok(s) => {
                let base = r
                    .reference_p50
                    .map_or(String::new(), |o| format!("  (cloud off p50 {o:.1} ms)"));
                format!("p50 {:.1} ms  p95 {:.1} ms{base}", s.p50_ms, s.p95_ms)
            }
            Outcome::Unsupported { reason } => format!("unsupported: {reason}"),
            Outcome::Error { reason } => format!("error: {reason}"),
        };
        t.push_str(&format!("  {:<28} {cell}\n", r.scenario));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(p50: f64, p95: f64) -> Stat {
        Stat {
            runs: 5,
            p50_ms: p50,
            p95_ms: p95,
        }
    }

    #[test]
    fn builtin_scenarios_parse_and_cover_the_budgets() {
        let s = scenarios();
        let kinds: Vec<_> = s.iter().map(|x| x.kind).collect();
        assert!(kinds.contains(&Kind::CommitLatency));
        assert!(kinds.contains(&Kind::TimeToVisible));
        assert!(kinds.contains(&Kind::PushUploaded));
        let v = s.iter().find(|x| x.kind == Kind::TimeToVisible).unwrap();
        assert_eq!(
            (v.budget.p50_ms, v.budget.p95_ms),
            (Some(2000.0), Some(10000.0))
        );
        assert!(v.files <= 10);
    }

    #[test]
    fn parse_rejects_bad_files() {
        let head = "[[scenario]]\nname='branchless-a'\n";
        assert!(
            parse("[[scenario]]\nname='x'\nkind='commit_latency'\nruns=1\nfiles=1\n[scenario.budget]\nnoise_pct=1.0\n")
                .is_err()
        );
        let case = |tail: &str| parse(&format!("{head}{tail}")).is_err();
        assert!(case(
            "kind='commit_latency'\nruns=0\nfiles=1\n[scenario.budget]\nnoise_pct=1.0\n"
        ));
        assert!(case(
            "kind='commit_latency'\nruns=1\nfiles=11\n[scenario.budget]\nnoise_pct=1.0\n"
        ));
        assert!(case("kind='time_to_visible'\nruns=1\nfiles=1\n"));
        assert!(case("kind='push_uploaded'\nruns=1\nfiles=1\nbogus=1\n"));
    }

    #[test]
    fn remote_must_be_https_or_ssh_without_password() {
        for ok in [
            "https://forge.example/o/r",
            "ssh://git@forge.example/o/r",
            "git@forge.example:o/r.git",
        ] {
            assert!(check_remote(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://forge.example/o/r",
            "git://forge.example/o/r",
            "file:///tmp/r",
            "ext::sh -c id",
            "ext::sh%20id",
            "--upload-pack=x",
            "https://user:secret@forge.example/o/r",
            "/local/path",
            "",
        ] {
            assert!(check_remote(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commit_budget_is_noise_tolerant_but_not_blind() {
        let b = Budget {
            noise_pct: Some(10.0),
            floor_ms: Some(5.0),
            ..Budget::default()
        };
        assert!(check_commit("c", &st(100.0, 120.0), &st(109.0, 130.0), &b).is_empty());
        assert_eq!(
            check_commit("c", &st(100.0, 120.0), &st(130.0, 150.0), &b).len(),
            1
        );
        // a big percentage on a tiny base stays inside the noise floor
        assert!(check_commit("c", &st(2.0, 3.0), &st(6.0, 7.0), &b).is_empty());
    }

    #[test]
    fn visible_budget_checks_both_percentiles() {
        let b = Budget {
            p50_ms: Some(2000.0),
            p95_ms: Some(10000.0),
            ..Budget::default()
        };
        assert!(check_visible("v", &st(1500.0, 9000.0), &b).is_empty());
        assert_eq!(check_visible("v", &st(2500.0, 9000.0), &b).len(), 1);
        assert_eq!(check_visible("v", &st(2500.0, 12000.0), &b).len(), 2);
    }

    #[test]
    fn baseline_regression_is_flagged() {
        let mut base = Baseline::default();
        base.entries.insert("cloud/p".into(), 100.0);
        assert!(check_baseline("p", &st(110.0, 0.0), &base).is_empty());
        assert_eq!(check_baseline("p", &st(130.0, 0.0), &base).len(), 1);
        assert!(check_baseline("other", &st(999.0, 0.0), &base).is_empty());
    }

    #[test]
    fn gate_fails_without_asc_unless_allowed() {
        let w = Path::new("unused-bench-dir-b25");
        let rows = run_cloud(None, Some("branchless"), None, &Baseline::default(), w);
        assert_eq!(rows.len(), 3);
        assert_eq!(gate_cloud(&rows, false).len(), 3);
        assert!(gate_cloud(&rows, true).is_empty());
    }

    #[test]
    fn bad_remote_is_an_error_row() {
        let w = Path::new("unused-bench-dir-b25");
        let rows = run_cloud(
            None,
            Some("time-to-visible"),
            Some("http://x/y"),
            &Baseline::default(),
            w,
        );
        assert!(
            matches!(&rows[0].outcome, Outcome::Error { reason } if reason.contains("ASC_BENCH_REMOTE"))
        );
        assert_eq!(gate_cloud(&rows, true).len(), 1);
    }
}
