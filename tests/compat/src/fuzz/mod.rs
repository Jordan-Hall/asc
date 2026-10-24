//! The differential fuzz harness (PLAN Q04, docs/git-fidelity.md "The checks").
//!
//! A seed makes a sequence of up to 30 steps over a small random tree (`ops`): files with text,
//! CRLF, binary and unicode content, renames, case-only renames, symlinks where the platform has
//! them, and the git commands that act on them. Each sequence runs in two fresh repositories, one
//! through the pinned oracle and one through the asc shim (`compare`), and everything the compat
//! runner compares is compared after every step that changes the repository. A difference is
//! dropped only by an `[[allow]]` entry of `allowed.toml` (each carries its register id) or
//! attributed to the machine by the preflight; any other difference fails with the seed and the
//! shrunken sequence (`shrink`), and is written as a draft scenario under
//! `tests/compat/fuzz-regressions/<seed>.t`, which `cargo test` replays from then on.
//!
//! A step that asc answers with `fatal: asc: ... is not supported` is removed from the sequence on
//! both sides and counted, so the harness improves as the native command table grows.
//!
//! Knobs (environment): `ASC_FUZZ_SEEDS` the number of sequences (default 25 for a PR run),
//! `ASC_FUZZ_SEED` the base seed (sequence `i` uses `base + i`, so one failing sequence replays with
//! `ASC_FUZZ_SEED=<its seed> ASC_FUZZ_SEEDS=1`), `ASC_FUZZ_OPS` the most steps per sequence (30),
//! `ASC_FUZZ_JOBS` the sequences run at once, `ASC_FUZZ_SHRINK` the most runs a shrink may use.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::harness::Tools;

pub mod compare;
pub mod ops;
pub mod shrink;

use compare::{Diff, Outcome};
use ops::Op;

/// The sequences of a PR run.
pub const DEFAULT_SEEDS: usize = 25;
/// The base seed of a default run, so that a PR run is the same run everywhere.
pub const DEFAULT_SEED: u64 = 20_261_023;
/// The most steps of one sequence.
pub const DEFAULT_OPS: usize = 30;

/// How a run is set up.
#[derive(Clone, Debug)]
pub struct Config {
    pub seeds: usize,
    pub base: u64,
    pub max_ops: usize,
    pub jobs: usize,
    pub shrink_budget: usize,
    /// Whether sequences may create symbolic links.
    pub symlinks: bool,
}

fn env_num<T: std::str::FromStr>(name: &str) -> Option<T> {
    let v = std::env::var(name).ok().filter(|v| !v.trim().is_empty())?;
    match v.trim().parse() {
        Ok(n) => Some(n),
        Err(_) => panic!("{name}={v:?} is not a number"),
    }
}

impl Config {
    pub fn from_env() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self {
            seeds: env_num("ASC_FUZZ_SEEDS").unwrap_or(DEFAULT_SEEDS),
            base: env_num("ASC_FUZZ_SEED").unwrap_or(DEFAULT_SEED),
            max_ops: env_num("ASC_FUZZ_OPS").unwrap_or(DEFAULT_OPS),
            jobs: env_num("ASC_FUZZ_JOBS")
                .unwrap_or_else(|| cpus.clamp(1, 4))
                .max(1),
            shrink_budget: env_num("ASC_FUZZ_SHRINK").unwrap_or(shrink::DEFAULT_BUDGET),
            symlinks: !cfg!(windows),
        }
    }
}

/// How one sequence ended.
pub enum CaseOutcome {
    Same,
    /// The preflight explains a difference.
    Adapter(String),
    /// The harness could not run the case.
    Broken(String),
    /// A difference no register entry covers, with the sequence shrunk to what it needs.
    Failed(Box<Failure>),
}

/// A difference, shrunk.
pub struct Failure {
    pub seed: u64,
    pub original_steps: usize,
    pub steps: Vec<Op>,
    pub diff: Diff,
}

/// One sequence, run.
pub struct CaseResult {
    pub seed: u64,
    pub outcome: CaseOutcome,
    /// The git commands asc did not serve in this sequence (those steps were removed).
    pub unserved: Vec<String>,
    /// The steps that ran on both sides.
    pub steps: usize,
}

/// The name a sequence is checked under (`allowed.toml` entries naming a scenario do not apply).
pub fn case_name(seed: u64) -> String {
    format!("fuzz-{seed}")
}

/// Runs the sequence of `seed`.
pub fn run_case(tools: &Tools, cfg: &Config, seed: u64) -> CaseResult {
    let name = case_name(seed);
    let mut steps = ops::generate(seed, cfg.max_ops, cfg.symlinks);
    let original_steps = steps.len();
    let mut unserved = Vec::new();
    let finish = |outcome, steps: usize, unserved: Vec<String>| CaseResult {
        seed,
        outcome,
        unserved,
        steps,
    };
    // An unserved step is dropped (on both sides) and the sequence runs again.
    for _ in 0..original_steps + 1 {
        match compare::check(tools, &name, &ops::script(&steps), None) {
            Outcome::Same => return finish(CaseOutcome::Same, steps.len(), unserved),
            Outcome::Adapter(m) => {
                return finish(CaseOutcome::Adapter(m), steps.len(), unserved);
            }
            Outcome::Broken(m) => return finish(CaseOutcome::Broken(m), steps.len(), unserved),
            Outcome::Unserved { op, command } => {
                unserved.push(command);
                if op >= steps.len() {
                    let m = format!(
                        "{name}: asc reported an unsupported step {op} of {}",
                        steps.len()
                    );
                    return finish(CaseOutcome::Broken(m), steps.len(), unserved);
                }
                steps.remove(op);
            }
            Outcome::Diff(diff) => {
                let failure = shrink_failure(tools, cfg, seed, original_steps, steps, diff);
                let n = failure.steps.len();
                return finish(CaseOutcome::Failed(Box::new(failure)), n, unserved);
            }
        }
    }
    let m = format!("{name}: too many unsupported steps");
    finish(CaseOutcome::Broken(m), steps.len(), unserved)
}

/// Shrinks `steps` while the same difference (the same signature) persists.
fn shrink_failure(
    tools: &Tools,
    cfg: &Config,
    seed: u64,
    original_steps: usize,
    steps: Vec<Op>,
    diff: Diff,
) -> Failure {
    let (steps, diff) = minimize(
        tools,
        &case_name(seed),
        steps,
        diff,
        cfg.shrink_budget,
        None,
    );
    Failure {
        seed,
        original_steps,
        steps,
        diff,
    }
}

/// The smallest sequence found that still has `diff` (the same signature): first cut after the
/// step of the first difference, then `shrink::shrink`. `tamper` alters the asc side as in
/// [`compare::check`]. Returns the sequence and its difference.
pub fn minimize(
    tools: &Tools,
    name: &str,
    mut steps: Vec<Op>,
    diff: Diff,
    budget: usize,
    tamper: Option<&dyn Fn(&mut crate::harness::Capture)>,
) -> (Vec<Op>, Diff) {
    let mut best = diff;
    let persists = |trial: &[Op], best: &mut Diff| match compare::check(
        tools,
        name,
        &ops::script(trial),
        tamper,
    ) {
        Outcome::Diff(d) if d.signature == best.signature => {
            *best = d;
            true
        }
        _ => false,
    };
    // Steps after the first difference cannot matter: try the sequence cut there first.
    if let Some(k) = best.op
        && k + 1 < steps.len()
        && persists(&steps[..=k], &mut best)
    {
        steps.truncate(k + 1);
    }
    let shrunk = shrink::shrink(steps, budget, |trial| persists(trial, &mut best));
    (shrunk, best)
}

/// The totals of a run.
#[derive(Default)]
pub struct Summary {
    pub cases: usize,
    pub same: usize,
    pub adapter: Vec<String>,
    pub broken: Vec<String>,
    pub failed: Vec<Failure>,
    /// The git commands asc did not serve, with the number of steps removed for each.
    pub unserved: BTreeMap<String, usize>,
    /// The steps that ran on both sides, over all sequences.
    pub steps: usize,
}

impl Summary {
    pub fn is_clean(&self) -> bool {
        self.adapter.is_empty() && self.broken.is_empty() && self.failed.is_empty()
    }

    /// The failures grouped by what differs (the signature), with their seeds.
    pub fn distinct(&self) -> BTreeMap<String, Vec<u64>> {
        let mut by: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for f in &self.failed {
            by.entry(f.diff.signature.replace('\n', " "))
                .or_default()
                .push(f.seed);
        }
        by
    }

    /// The final line of a run.
    pub fn line(&self, cfg: &Config) -> String {
        let unserved: usize = self.unserved.values().sum();
        let mut out = format!(
            "diff_fuzz: seed={} cases={} steps={} same={} failed={} adapter={} broken={} \
             unserved={unserved}",
            cfg.base,
            self.cases,
            self.steps,
            self.same,
            self.failed.len(),
            self.adapter.len(),
            self.broken.len(),
        );
        if !self.unserved.is_empty() {
            let each: Vec<String> = self
                .unserved
                .iter()
                .map(|(c, n)| format!("{c} x{n}"))
                .collect();
            let _ = write!(out, " ({})", each.join(", "));
        }
        out
    }
}

/// Runs `cfg.seeds` sequences, `cfg.jobs` at a time.
pub fn run(tools: &Tools, cfg: &Config) -> Summary {
    println!(
        "diff_fuzz: base seed {} ({} sequences of up to {} steps; replay with ASC_FUZZ_SEED={} \
         ASC_FUZZ_SEEDS={})",
        cfg.base, cfg.seeds, cfg.max_ops, cfg.base, cfg.seeds
    );
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<CaseResult>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..cfg.jobs.min(cfg.seeds).max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= cfg.seeds {
                        break;
                    }
                    let r = run_case(tools, cfg, cfg.base.wrapping_add(i as u64));
                    if let Ok(mut all) = results.lock() {
                        all.push(r);
                    }
                }
            });
        }
    });
    let mut all = results.into_inner().unwrap_or_default();
    all.sort_by_key(|r| r.seed.wrapping_sub(cfg.base));
    let mut sum = Summary::default();
    for r in all {
        sum.cases += 1;
        sum.steps += r.steps;
        for c in r.unserved {
            *sum.unserved.entry(c).or_default() += 1;
        }
        match r.outcome {
            CaseOutcome::Same => sum.same += 1,
            CaseOutcome::Adapter(m) => sum.adapter.push(m),
            CaseOutcome::Broken(m) => sum.broken.push(m),
            CaseOutcome::Failed(f) => sum.failed.push(*f),
        }
    }
    sum
}

/// `tests/compat/fuzz-regressions`.
pub fn regressions_dir(tools: &Tools) -> PathBuf {
    tools.suite.join("fuzz-regressions")
}

/// The draft scenario of a failure: the shrunken script under a comment that says where it came
/// from. Replaying it (`cargo test -p asc --test diff_fuzz regressions_replay`) fails until the
/// difference is fixed or registered.
pub fn regression_text(f: &Failure) -> String {
    let mut out = format!(
        "# fuzz regression, seed {} (docs/git-fidelity.md, Q04): a difference no register entry covers.\n\
         # first differing section: {}\n# {} of {} steps remain after shrinking.\n",
        f.seed,
        f.diff.signature.replace('\n', " "),
        f.steps.len(),
        f.original_steps,
    );
    out.push_str(&ops::script(&f.steps));
    out
}

/// Writes the draft scenario of a failure to `dir/<seed>.t`.
pub fn write_regression(dir: &std::path::Path, f: &Failure) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.t", f.seed));
    fs::write(&path, regression_text(f))?;
    Ok(path)
}

/// The failure report of one shrunken sequence.
pub fn report(f: &Failure, written: Option<&std::path::Path>) -> String {
    let mut out = format!(
        "seed {}: asc differs from the oracle; {} of {} steps remain after shrinking\n\
         first difference: {}\n",
        f.seed,
        f.steps.len(),
        f.original_steps,
        f.diff.signature.replace('\n', " "),
    );
    if let Some(op) = f.diff.op {
        let _ = writeln!(out, "at {}", ops::describe(&f.steps, op));
    }
    out.push_str("steps:\n");
    for i in 0..f.steps.len() {
        let _ = writeln!(out, "  {}", ops::describe(&f.steps, i));
    }
    let _ = writeln!(
        out,
        "replay: ASC_FUZZ_SEED={} ASC_FUZZ_SEEDS=1 cargo test -p asc --test diff_fuzz differential_fuzz",
        f.seed
    );
    if let Some(p) = written {
        let _ = writeln!(out, "draft scenario: {}", p.display());
    }
    out.push_str(&f.diff.report);
    out
}

/// Replays every draft scenario under `fuzz-regressions`; the errors of those that still differ.
pub fn replay(tools: &Tools) -> Vec<String> {
    let dir = regressions_dir(tools);
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "t"))
        .collect();
    files.sort();
    let mut errors = Vec::new();
    for path in files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|text| {
                tools.check_script(&format!("fuzz-{stem}"), &text.replace("\r\n", "\n"))
            });
        if let Err(e) = result {
            errors.push(format!("fuzz-regressions/{stem}.t: {e}"));
        }
    }
    errors
}
