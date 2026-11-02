//! B21 acceptance: the git compatibility suite in both branchless modes (SPEC §17.1, §21.1 BD7).
//!
//! The PLAN row names `tests/compat/tests/modes.rs` and `cargo test -p asc-compat --test modes`;
//! the harness is included by the `asc` test crates (see `compat.rs`), so the test lives here:
//! `cargo test -p asc --test compat_modes`.
//!
//! Every scenario that is not `asc-only` runs on the asc side with `asc.branchless=true` and with
//! `asc.branchless=false`. Each run must match real git and the two runs must be byte-identical,
//! both up to `allowed.toml`. Scenarios that already carry the marker are run as written.

#[path = "../../../tests/compat/src/lib.rs"]
mod harness;

use harness::Tools;

fn tools() -> &'static Tools {
    match Tools::get(env!("CARGO_BIN_EXE_asc")) {
        Ok(t) => t,
        Err(e) => panic!("compat tools: {e}"),
    }
}

const MARKER: &str = "# compat: branchless=both";

/// `script` with the both-modes marker on its first line, unless it has one or is `asc-only`.
fn in_both_modes(script: &str) -> Option<String> {
    if script
        .lines()
        .take(5)
        .any(|l| l.trim() == "# compat: asc-only")
    {
        return None;
    }
    if harness::branchless_modes(script).len() == 2 {
        return Some(script.to_owned());
    }
    Some(format!("{MARKER}\n{script}"))
}

#[test]
fn the_marker_turns_a_scenario_into_two_asc_runs() {
    let plain = "g init\n";
    let forced = in_both_modes(plain).expect("plain scenario");
    assert_eq!(
        harness::branchless_modes(&forced),
        vec![Some(true), Some(false)]
    );
    assert_eq!(in_both_modes(&forced).as_deref(), Some(forced.as_str()));
    assert!(in_both_modes("# compat: asc-only\ng init\n").is_none());
}

#[test]
fn branchless_push_runs_in_both_modes() {
    let script = tools().scenario("branchless-push").expect("scenario");
    assert_eq!(harness::branchless_modes(&script).len(), 2);
    tools()
        .check_script("branchless-push", &script)
        .unwrap_or_else(|e| panic!("{e}"));
}

/// Scenarios per group. One test runs one group, so a slow machine fails one group naming what it
/// did not reach and the other groups still report; the groups also run side by side.
const GROUP_SIZE: usize = 20;
/// Groups the file declares. [`every_scenario_belongs_to_a_group`] fails when the scenarios outgrow
/// them, so a new scenario cannot go unrun.
const GROUPS: usize = 6;

/// The sorted names of the scenarios that run in both modes' sweep, `ASC_MODES_FILTER` applied.
fn sweep_names() -> Vec<String> {
    let filter = std::env::var("ASC_MODES_FILTER").unwrap_or_default();
    tools()
        .scenario_names()
        .into_iter()
        .filter(|n| n.contains(&filter))
        .collect()
}

#[test]
fn every_scenario_belongs_to_a_group() {
    let n = sweep_names().len();
    assert!(
        n <= GROUPS * GROUP_SIZE,
        "{n} scenarios do not fit {GROUPS} groups of {GROUP_SIZE}: add a `group!` line and raise GROUPS"
    );
}

/// Runs group `index` (scenarios `index * GROUP_SIZE ..`) in both modes.
///
/// `ASC_MODES_FILTER=<substring>` limits the sweep first. Each group has its own wall-clock budget,
/// `ASC_MODES_BUDGET_SECS` (default 1800): one scenario may take `SCRIPT_TIMEOUT` on each side and
/// each side runs more than once, so a group has no useful upper bound of its own. When the budget
/// is spent the group stops and FAILS naming every scenario it did not reach, so a slow or hung
/// machine is a visible failure and not an hour of silence. CI runs all groups; a development
/// machine with slow process start-up (a debug build on a network or 9p mount) can run one group
/// with `cargo test -p asc --test compat_modes group_2`.
fn run_group(index: usize) {
    let budget = std::time::Duration::from_secs(
        std::env::var("ASC_MODES_BUDGET_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1800),
    );
    let names = sweep_names();
    let group: Vec<String> = names
        .into_iter()
        .skip(index * GROUP_SIZE)
        .take(GROUP_SIZE)
        .collect();
    let started = std::time::Instant::now();
    let mut failures = Vec::new();
    let mut unreached = Vec::new();
    let mut ran = 0;
    for name in group {
        if started.elapsed() > budget {
            unreached.push(name);
            continue;
        }
        let script = tools().scenario(&name).expect("scenario");
        let Some(script) = in_both_modes(&script) else {
            continue;
        };
        ran += 1;
        if let Err(e) = tools().check_script(&name, &script) {
            failures.push(e);
        }
    }
    assert!(
        unreached.is_empty(),
        "group {index}: the {}s budget (ASC_MODES_BUDGET_SECS) ran out after {ran} scenarios; not run: {}",
        budget.as_secs(),
        unreached.join(", ")
    );
    assert!(
        failures.is_empty(),
        "group {index}: {} of {ran} scenarios differ between modes:
{}",
        failures.len(),
        failures.join(
            "

"
        )
    );
}

macro_rules! group {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            run_group($index);
        }
    };
}

group!(scenarios_group_0_are_byte_identical_in_both_modes, 0);
group!(scenarios_group_1_are_byte_identical_in_both_modes, 1);
group!(scenarios_group_2_are_byte_identical_in_both_modes, 2);
group!(scenarios_group_3_are_byte_identical_in_both_modes, 3);
group!(scenarios_group_4_are_byte_identical_in_both_modes, 4);
group!(scenarios_group_5_are_byte_identical_in_both_modes, 5);
