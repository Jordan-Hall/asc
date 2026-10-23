//! asc-sim: hermetic simulation (test-only), SPEC 17.2.
//!
//! One process runs N `asc serve` replicas (primary and cache role), M clients, a fake Forgejo and
//! an in-memory store, all driven by one seeded RNG and a virtual clock. There are no threads, no
//! sockets and no wall-clock reads, so a seed replays exactly: the trace hash of two runs of one
//! seed is equal. The store is the real `BucketObjects` (in memory) behind `Faulty<O>`; refs are
//! an in-memory model of the CAS `Refs` trait behind `FaultyRefs`.
//!
//! Faults: added latency, `UNAVAILABLE`, a lost response after a successful write, a partition
//! (`FaultPlan::down`), and replica crashes at the named points of [`faults::CrashPoint`].
//!
//! Run: `ASC_SIM_SEEDS=200 cargo test -p asc-sim` (PR), `ASC_SIM_SEEDS=10000` (nightly). A failure
//! prints `ASC_SIM_SEED=<n> ASC_SIM_SCENARIO=<name>`; set both to replay it.

pub mod cache;
pub mod faults;
pub mod gc;
pub mod import;
pub mod invariants;
pub mod jobs;
pub mod memrefs;
pub mod model;
pub mod rng;
pub mod scenarios;
pub mod world;

use std::fmt;

pub use model::Bugs;
pub use scenarios::{Report, Scenario};

/// Seeds a PR run covers.
pub const PR_SEEDS: u64 = 200;
/// Seeds the nightly run covers.
pub const NIGHTLY_SEEDS: u64 = 10_000;
/// Seeds when `ASC_SIM_SEEDS` is unset.
pub const DEFAULT_SEEDS: u64 = 25;

/// A failed run: what broke, where, and how to replay it.
#[derive(Clone, Debug)]
pub struct SimFailure {
    pub scenario: Scenario,
    pub seed: u64,
    pub step: u64,
    pub message: String,
}

impl fmt::Display for SimFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "asc-sim FAILED scenario={} seed={} step={}: {}\nreplay: ASC_SIM_SEED={} ASC_SIM_SCENARIO={} cargo test -p asc-sim",
            self.scenario.name(),
            self.seed,
            self.step,
            self.message,
            self.seed,
            self.scenario.name()
        )
    }
}

impl std::error::Error for SimFailure {}

/// Runs one scenario on one seed.
pub fn run_one(scenario: Scenario, seed: u64, bugs: Bugs) -> Result<Report, SimFailure> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| SimFailure {
            scenario,
            seed,
            step: 0,
            message: format!("runtime: {e}"),
        })?;
    rt.block_on(scenarios::run(scenario, seed, bugs))
        .map_err(|(step, fail)| SimFailure {
            scenario,
            seed,
            step,
            message: match fail {
                model::Fail::Violation(m) => m,
                model::Fail::Transient => "a transient error escaped the scenario".into(),
                model::Fail::Crashed(p) => format!("an unhandled crash at {}", p.name()),
            },
        })
}

/// What a test run covers, from the environment.
#[derive(Clone, Debug)]
pub struct Plan {
    pub seeds: Vec<u64>,
    pub scenarios: Vec<Scenario>,
}

impl Plan {
    /// `ASC_SIM_SEED` replays one seed; otherwise `ASC_SIM_SEEDS` (default 25) seeds from 1.
    /// `ASC_SIM_SCENARIO` limits the run to one scenario.
    pub fn from_env() -> Self {
        let seeds = match std::env::var("ASC_SIM_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(one) => vec![one],
            None => {
                let n = std::env::var("ASC_SIM_SEEDS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_SEEDS);
                (1..=n).collect()
            }
        };
        let scenarios = std::env::var("ASC_SIM_SCENARIO")
            .ok()
            .and_then(|s| Scenario::parse(&s))
            .map_or_else(|| Scenario::ALL.to_vec(), |s| vec![s]);
        Self { seeds, scenarios }
    }
}

/// Runs a plan; stops at the first failure.
pub fn run_plan(plan: &Plan) -> Result<Vec<Report>, SimFailure> {
    eprintln!(
        "asc-sim: {} seed(s) x {} scenario(s); a failure prints its seed (replay with ASC_SIM_SEED)",
        plan.seeds.len(),
        plan.scenarios.len()
    );
    let mut out = Vec::new();
    for &seed in &plan.seeds {
        for &sc in &plan.scenarios {
            match run_one(sc, seed, Bugs::default()) {
                Ok(r) => out.push(r),
                Err(f) => {
                    eprintln!("{f}");
                    return Err(f);
                }
            }
        }
    }
    Ok(out)
}
