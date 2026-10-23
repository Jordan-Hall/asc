//! Q01 acceptance: the git-fidelity lint (`docs/git-fidelity.md`, "The checks") on the real
//! registers, and its self-checks. The lint lives in `tests/compat/src/fidelity.rs`, next to the
//! compat runner, and is included here the way `compat.rs` includes the runner.

#[path = "../../../tests/compat/src/fidelity.rs"]
mod fidelity;

use std::path::{Path, PathBuf};

use fidelity::{Register, Report, Texts, lint, parse_register, strict_from_env};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn real() -> Texts {
    Texts::read(&root()).expect("the registers")
}

/// A quirk section, built field by field so a self-check can alter one.
fn quirk(id: &str, fields: &[(&str, &str)]) -> String {
    let mut s = format!("\n### {id} a made-up entry\n");
    for (k, v) in fields {
        s.push_str(&format!("- **{k}:** {v}\n"));
    }
    s
}

/// The fields of a valid `replicate` quirk whose proof is this file.
fn replicate_fields<'a>() -> Vec<(&'a str, &'a str)> {
    vec![
        ("Git", "what git does"),
        ("Ideal", "what the docs say"),
        ("Decision", "replicate"),
        ("Proof", "`crates/asc/tests/fidelity.rs`"),
        ("Status", "proven"),
        ("SPEC", "§11.1"),
    ]
}

fn with_quirk(extra: &str) -> Texts {
    let mut t = real();
    t.quirks.push_str(extra);
    t
}

fn run(t: &Texts) -> Report {
    lint(&root(), t, false)
}

fn has(r: &Report, needle: &str) -> bool {
    r.errors.iter().any(|e| e.contains(needle))
}

#[test]
fn the_real_registers_pass_the_lint() {
    let report = lint(&root(), &real(), strict_from_env());
    for l in &report.listed {
        println!("fidelity: listed {l}");
    }
    assert!(report.errors.is_empty(), "{}", report.problems());
}

/// The seeded registers (2026-10-23): ids are never reused or renumbered, so these stay.
#[test]
fn the_seeded_entries_parse() {
    let t = real();
    let count = |r: Register| {
        parse_register(
            r,
            match r {
                Register::Quirk => &t.quirks,
                Register::Defect => &t.defects,
                Register::Divergence => &t.divergences,
                Register::Gap => &t.gaps,
            },
        )
    };
    for (reg, ids) in [
        (
            Register::Quirk,
            (1..=8).map(|n| format!("Q-{n}")).collect::<Vec<_>>(),
        ),
        (
            Register::Defect,
            (1..=2).map(|n| format!("U-{n}")).collect(),
        ),
        (
            Register::Divergence,
            (1..=7).map(|n| format!("D-{n}")).collect(),
        ),
        (Register::Gap, (1..=5).map(|n| format!("A-{n}")).collect()),
    ] {
        let parsed = count(reg);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let have: Vec<_> = parsed.entries.iter().map(|e| e.id.clone()).collect();
        for id in ids {
            assert!(have.contains(&id), "{id} missing from {have:?}");
        }
    }
    let report = run(&t);
    assert!(report.entries.len() >= 22, "{}", report.entries.len());
    // The four [[allow]] entries of allowed.toml, in file order.
    let regs: Vec<_> = report.allow_registers.iter().flatten().cloned().collect();
    assert_eq!(regs, ["Q-1", "D-3", "D-1", "D-2"]);
}

#[test]
fn a_duplicated_id_fails() {
    let t = with_quirk(&quirk("Q-1", &replicate_fields()));
    let r = run(&t);
    assert!(
        has(&r, "Q-1") && has(&r, "duplicate id"),
        "{}",
        r.problems()
    );
}

#[test]
fn an_id_in_the_wrong_register_fails() {
    let mut t = real();
    t.defects.push_str(&quirk("Q-93", &replicate_fields()));
    let r = run(&t);
    assert!(
        has(&r, "does not belong in this register"),
        "{}",
        r.problems()
    );
}

#[test]
fn two_decisions_fail() {
    let mut f = replicate_fields();
    f.push(("Decision", "refuse"));
    let r = run(&with_quirk(&quirk("Q-90", &f)));
    assert!(
        has(&r, "Q-90") && has(&r, "exactly one Decision"),
        "{}",
        r.problems()
    );
}

#[test]
fn an_unknown_decision_fails() {
    let mut f = replicate_fields();
    f.retain(|(k, _)| *k != "Decision");
    f.push(("Decision", "ignore"));
    let r = run(&with_quirk(&quirk("Q-94", &f)));
    assert!(
        has(&r, "Q-94") && has(&r, "exactly one Decision"),
        "{}",
        r.problems()
    );
}

#[test]
fn an_allow_entry_citing_a_missing_id_fails() {
    let mut t = real();
    t.allowed.push_str(
        "\n[[allow]]\nscenario = \"x\"\nsection = \"stdout\"\ndrop = '^x$'\nreason = \"r\"\nspec = \"§1.1\"\nregister = \"Q-999\"\n",
    );
    let r = run(&t);
    assert!(
        has(&r, "Q-999") && has(&r, "does not exist"),
        "{}",
        r.problems()
    );
}

#[test]
fn an_allow_entry_without_a_register_fails() {
    let mut t = real();
    t.allowed.push_str(
        "\n[[allow]]\nscenario = \"x\"\nsection = \"stdout\"\ndrop = '^x$'\nreason = \"r\"\nspec = \"§1.1\"\n",
    );
    let r = run(&t);
    assert!(has(&r, "no `register` key"), "{}", r.problems());
}

#[test]
fn a_differing_trace_without_a_register_fails() {
    let mut t = real();
    t.pending.push_str(
        "\n[[pending]]\ntrace = \"x/y\"\nunit = \"U01\"\nreason = \"r\"\ndiffers = true\n",
    );
    let r = run(&t);
    assert!(
        has(&r, "x/y") && has(&r, "needs a `register` key"),
        "{}",
        r.problems()
    );
}

#[test]
fn a_proven_entry_with_no_marker_fails() {
    let r = run(&with_quirk(&quirk("Q-91", &replicate_fields())));
    assert!(
        has(&r, "Q-91") && has(&r, "no file cites it"),
        "{}",
        r.problems()
    );
}

#[test]
fn a_proof_file_that_is_missing_fails() {
    let mut f = replicate_fields();
    f.retain(|(k, _)| *k != "Proof");
    f.push(("Proof", "`crates/asc/tests/no_such_file.rs::nothing`"));
    let r = run(&with_quirk(&quirk("Q-92", &f)));
    assert!(
        has(&r, "Q-92") && has(&r, "does not exist"),
        "{}",
        r.problems()
    );
}

#[test]
fn a_proof_test_that_is_missing_fails() {
    let mut f = replicate_fields();
    f.retain(|(k, _)| *k != "Proof");
    // The name is kept apart from "fn": the lint looks for the text in this very file.
    let name = "absent_test";
    let proof = format!("`crates/asc/tests/fidelity.rs::{name}`");
    f.push(("Proof", &proof));
    let r = run(&with_quirk(&quirk("Q-95", &f)));
    assert!(has(&r, &format!("has no fn {name}")), "{}", r.problems());
}

#[test]
fn a_refuse_needs_a_refusal_kind_and_an_upstream_bullet() {
    let mut f = replicate_fields();
    f.retain(|(k, _)| *k != "Decision" && *k != "Status");
    f.push(("Decision", "refuse"));
    f.push(("Status", "open"));
    let r = run(&with_quirk(&quirk("Q-96", &f)));
    assert!(
        has(&r, "Q-96") && has(&r, "needs one Refusal"),
        "{}",
        r.problems()
    );
    assert!(has(&r, "no bullet under `## Upstream`"), "{}", r.problems());
}

#[test]
fn a_refuse_with_an_error_needs_its_id_in_source() {
    let mut f = replicate_fields();
    f.retain(|(k, _)| *k != "Decision" && *k != "Status");
    f.push(("Decision", "refuse"));
    f.push(("Refusal", "error"));
    f.push(("Status", "open"));
    let mut t = with_quirk(&quirk("Q-97", &f));
    t.divergences = t.divergences.replacen(
        "\n- **Q-3** (refuse)",
        "\n- **Q-97** (refuse) made up.\n- **Q-3** (refuse)",
        1,
    );
    let r = run(&t);
    assert!(
        has(&r, "Q-97") && has(&r, "git-fidelity Q-97"),
        "{}",
        r.problems()
    );
}

#[test]
fn an_upstream_bullet_citing_a_replicate_fails() {
    let mut t = real();
    t.divergences = t.divergences.replacen(
        "\n- **Q-3** (refuse)",
        "\n- **Q-4** (refuse) made up.\n- **Q-3** (refuse)",
        1,
    );
    let r = run(&t);
    assert!(
        has(&r, "cites Q-4") && has(&r, "replicate"),
        "{}",
        r.problems()
    );
}

#[test]
fn an_upstream_bullet_citing_a_missing_id_fails() {
    let mut t = real();
    t.divergences = t.divergences.replacen(
        "\n- **Q-3** (refuse)",
        "\n- **Q-998** (refuse) made up.\n- **Q-3** (refuse)",
        1,
    );
    let r = run(&t);
    assert!(
        has(&r, "Q-998") && has(&r, "in no register"),
        "{}",
        r.problems()
    );
}

#[test]
fn a_divergence_without_a_class_fails() {
    let mut t = real();
    t.divergences = t
        .divergences
        .replacen("- **Class:** auth", "- **Kind:** auth", 1);
    let r = run(&t);
    assert!(has(&r, "exactly one Class"), "{}", r.problems());
}

#[test]
fn a_gap_with_two_kinds_fails() {
    let mut t = real();
    t.gaps = t.gaps.replacen(
        "- **Kind:** blind",
        "- **Kind:** blind\n- **Kind:** drift",
        1,
    );
    let r = run(&t);
    assert!(has(&r, "exactly one Kind"), "{}", r.problems());
}

/// Pending and open entries pass the default run, are listed, and fail the strict run.
#[test]
fn strict_mode_fails_on_pending_and_open_entries() {
    let t = real();
    let default = lint(&root(), &t, false);
    assert!(default.errors.is_empty(), "{}", default.problems());
    assert!(
        default.listed.iter().any(|l| l.starts_with("D-7")),
        "{:?}",
        default.listed
    );
    let strict = lint(&root(), &t, true);
    assert!(has(&strict, "ASC_FIDELITY_STRICT"), "{}", strict.problems());
}
