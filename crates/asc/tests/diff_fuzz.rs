//! Q04 acceptance: the differential fuzz against the pinned git oracle (docs/git-fidelity.md,
//! "The checks"). The harness lives in `tests/compat/src/fuzz/`; it drives the compat runner
//! (`tests/compat/src/lib.rs`) with generated scripts instead of the files in `scenarios/`.
//!
//! `ASC_FUZZ_SEEDS=25 cargo test -p asc --test diff_fuzz` is the PR run; see `fuzz/mod.rs` for the
//! knobs (`ASC_FUZZ_SEED` replays one failing sequence).

#[path = "../../../tests/compat/src/lib.rs"]
mod harness;

#[path = "../../../tests/compat/src/fuzz/mod.rs"]
mod fuzz;

use fuzz::compare::{self, Outcome};
use fuzz::ops::{self, Op};
use harness::Tools;

fn tools() -> &'static Tools {
    match Tools::get(env!("CARGO_BIN_EXE_asc")) {
        Ok(t) => t,
        Err(e) => panic!("compat tools: {e}"),
    }
}

/// The fuzz: generated sequences against the oracle. Every difference must be covered by an
/// `allowed.toml` entry (with its register id) or the run fails with the shrunken sequence, which
/// is also written to `tests/compat/fuzz-regressions/<seed>.t`.
#[test]
fn differential_fuzz() {
    let tools = tools();
    let cfg = fuzz::Config::from_env();
    let sum = fuzz::run(tools, &cfg);
    println!("{}", sum.line(&cfg));
    if sum.is_clean() {
        return;
    }
    let mut msg = format!("{}\n", sum.line(&cfg));
    for m in sum.adapter.iter().chain(&sum.broken) {
        msg.push_str(m);
        msg.push('\n');
    }
    for (what, seeds) in sum.distinct() {
        msg.push_str(&format!("distinct difference {what:?}: seeds {seeds:?}\n"));
    }
    for f in &sum.failed {
        let written = fuzz::write_regression(&fuzz::regressions_dir(tools), f).ok();
        msg.push_str(&fuzz::report(f, written.as_deref()));
    }
    panic!("{msg}");
}

/// Draft scenarios written by earlier failures run again, so a difference stays visible until it
/// is fixed or registered.
#[test]
fn regressions_replay() {
    let errors = fuzz::replay(tools());
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn generator_is_seeded_and_bounded() {
    for seed in [0u64, 1, 7, 20_261_023, u64::MAX] {
        let a = ops::generate(seed, 30, true);
        assert_eq!(
            a,
            ops::generate(seed, 30, true),
            "seed {seed} is not repeatable"
        );
        assert!(
            (4..=30).contains(&a.len()),
            "seed {seed}: {} steps",
            a.len()
        );
        assert_eq!(ops::script(&a), ops::script(&ops::generate(seed, 30, true)));
    }
    assert_ne!(ops::generate(1, 30, true), ops::generate(2, 30, true));
    assert!(ops::generate(3, 8, true).len() <= 8);
}

/// Over many seeds the vocabulary is all there: the commands of the brief, binary and CRLF
/// content, unicode and case-only names, and symlinks when allowed.
#[test]
fn generator_covers_the_vocabulary() {
    let mut text = String::new();
    for seed in 0..400 {
        text.push_str(&ops::script(&ops::generate(seed, 30, true)));
    }
    for want in [
        "g 'add'",
        "g 'commit'",
        "g 'branch'",
        "g 'switch'",
        "g 'checkout'",
        "g 'merge'",
        "g 'rebase'",
        "g 'rebase' '-i'",
        "g 'cherry-pick'",
        "g 'revert'",
        "g 'stash'",
        "g 'tag'",
        "g 'mv'",
        "g 'rm'",
        "g 'reset'",
        "g 'restore'",
        "g 'blame'",
        "g 'show'",
        "g 'for-each-ref'",
        "g 'ls-files'",
        "g 'rev-parse'",
        "'--amend'",
        "'--allow-empty'",
        "'--no-ff'",
        "'--ff-only'",
        "'--soft'",
        "'--hard'",
        "'tag' '-a'",
        "ln -s",
        "\\000",
        "\\015\\n",
        "'\u{e9}.txt'",
        "'Case.txt'",
        "GIT_SEQUENCE_EDITOR",
        "chmod +x",
    ] {
        assert!(text.contains(want), "no generated script contains {want}");
    }
    // Without symlinks none is generated.
    let mut plain = String::new();
    for seed in 0..200 {
        plain.push_str(&ops::script(&ops::generate(seed, 30, false)));
    }
    assert!(!plain.contains("ln -s"));
}

/// A script that sets the asc side apart from the oracle is found and shrunk to the steps that
/// show it. The alteration: asc's `for-each-ref` lines name `refs/tagz/` where the oracle has
/// `refs/tags/`, which needs a commit and a tag.
#[test]
fn tampered_asc_output_is_caught_and_shrunk() {
    let tools = tools();
    let name = "fuzz-self-check";
    let tamper = |asc: &mut harness::Capture| {
        for key in ["stdout", "post:for-each-ref"] {
            if let Some(b) = asc.sections.get_mut(key) {
                let s = String::from_utf8_lossy(b).replace("refs/tags/", "refs/tagz/");
                *b = s.into_bytes();
            }
        }
    };
    // A fixed sequence (the generator is not what is under test here): a commit, a
    // second branch and a tag, which is what the alteration needs.
    let step = |label: &'static str, mutating: bool, lines: &[&str]| Op {
        label,
        sh: lines.iter().map(|l| (*l).to_owned()).collect(),
        mutating,
    };
    let steps = vec![
        step("write", false, &["printf 'one\n' > a.txt"]),
        step("add", true, &["g add -A"]),
        step("commit", true, &["g commit -m one"]),
        step("branch", true, &["g branch side"]),
        step("tag", true, &["g tag self-check"]),
        step("switch", true, &["g switch side"]),
    ];
    let first = match compare::check(tools, name, &ops::script(&steps), Some(&tamper)) {
        Outcome::Diff(d) => d,
        other => panic!("the altered output was not reported as a difference: {other:?}"),
    };
    assert!(
        first.report.contains("refs/tagz/"),
        "the report does not show the alteration:\n{}",
        first.report
    );
    let (shrunk, _) = fuzz::minimize(tools, name, steps.clone(), first, 30, Some(&tamper));
    assert!(
        shrunk.len() < steps.len() && shrunk.len() <= 4,
        "{} steps remain of {}:\n{}",
        shrunk.len(),
        steps.len(),
        (0..shrunk.len())
            .map(|i| ops::describe(&shrunk, i))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        shrunk.iter().any(|o| o.label == "tag"),
        "the tag step is what shows the alteration"
    );
    // The same sequence without the alteration agrees.
    assert!(matches!(
        compare::check(tools, name, &ops::script(&shrunk), None),
        Outcome::Same
    ));
}

/// The script of Q-1 (an external `git-hello` against `alias.hello`): the allowed difference is
/// dropped under the scenario name `allowed.toml` gives it, and is a difference under any other.
#[test]
fn an_allowed_difference_is_dropped() {
    let tools = tools();
    let script = "# fidelity: Q-1\n\
        mkdir ../bin\n\
        printf '#!/bin/sh\\necho \"hello from external: $*\"\\n' > ../bin/git-hello\n\
        chmod +x ../bin/git-hello\n\
        PATH=\"$(cd .. && pwd)/bin:$PATH\"\n\
        export PATH\n\
        g init\n\
        g config alias.hello \"log -1 --format=alias-wins\"\n\
        echo a > a.txt\n\
        g add a.txt\n\
        at 1\n\
        g commit -m first\n\
        g hello\n";
    match compare::check(tools, "external-commands", script, None) {
        Outcome::Same => {}
        other => panic!("the Q-1 difference was not dropped: {other:?}"),
    }
    match compare::check(tools, "fuzz-not-allowed", script, None) {
        Outcome::Diff(d) => assert!(d.sections.iter().any(|s| s == "stdout"), "{d:?}"),
        other => panic!("Q-1 differs without its allowed entry, got {other:?}"),
    }
}

#[test]
fn summary_line_counts_unserved_commands() {
    let cfg = fuzz::Config {
        seeds: 3,
        base: 9,
        max_ops: 30,
        jobs: 1,
        shrink_budget: 1,
        symlinks: true,
    };
    let mut sum = fuzz::Summary {
        cases: 3,
        same: 3,
        steps: 40,
        ..fuzz::Summary::default()
    };
    sum.unserved.insert("rebase".into(), 2);
    sum.unserved.insert("blame".into(), 1);
    let line = sum.line(&cfg);
    assert!(line.contains("unserved=3 (blame x1, rebase x2)"), "{line}");
    assert!(line.contains("seed=9 cases=3"), "{line}");
}
