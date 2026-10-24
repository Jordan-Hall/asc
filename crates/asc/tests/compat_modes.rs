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

/// `ASC_MODES_FILTER=<substring>` limits the sweep; unset runs every scenario.
#[test]
fn every_scenario_is_byte_identical_in_both_modes() {
    let filter = std::env::var("ASC_MODES_FILTER").unwrap_or_default();
    let mut failures = Vec::new();
    let mut ran = 0;
    for name in tools().scenario_names() {
        if !name.contains(&filter) {
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
    assert!(ran > 0 || !filter.is_empty(), "no scenario ran");
    assert!(
        failures.is_empty(),
        "{} of {ran} scenarios differ between modes:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
