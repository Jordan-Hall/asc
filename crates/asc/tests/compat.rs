//! U25 acceptance: the git compatibility suite (SPEC §17.1) for the local-only milestone.
//! The runner lives in `tests/compat/src/lib.rs`; the cases are `tests/compat/scenarios/*.t`.

#[path = "../../../tests/compat/src/lib.rs"]
mod harness;

use harness::Tools;

fn tools() -> &'static Tools {
    match Tools::get(env!("CARGO_BIN_EXE_asc")) {
        Ok(t) => t,
        Err(e) => panic!("compat tools: {e}"),
    }
}

fn scenario(name: &str) {
    if let Err(e) = tools().check_scenario(name) {
        panic!("{e}");
    }
}

/// One test per scenario, so `cargo test -p asc --test compat -- <name>` selects it. Add a line
/// here with each new `scenarios/<name>.t`; `every_scenario_has_a_test` fails otherwise.
macro_rules! scenarios {
    ($($test:ident = $file:literal),* $(,)?) => {
        $( #[test] fn $test() { scenario($file); } )*
        const REGISTERED: &[&str] = &[$($file),*];
    };
}

scenarios! {
    init_add_commit = "init-add-commit",
    status_variants = "status-variants",
    branch_switch = "branch-switch",
    diff_log = "diff-log",
    reset_restore = "reset-restore",
    tag = "tag",
    rename = "rename",
    ignore = "ignore",
    ascignore = "ascignore",
    config_alias_includeif = "config-alias-includeif",
    external_commands = "external-commands",
    commit_variants = "commit-variants",
    hooks = "hooks",
    merge_ff = "merge-ff",
    pick_options = "pick-options",
    merge_misc = "merge-misc",
    merge_conflict = "merge-conflict",
    merge_dirty = "merge-dirty",
    merge_hooks = "merge-hooks",
    merge_lintstaged = "merge-lintstaged",
    merge_driver = "merge-driver",
    pick_basic = "pick-basic",
    grep_rev = "grep-rev",
    pick_sequence = "pick-sequence",
    pick_empty = "pick-empty",
    rebase_basic = "rebase-basic",
    rebase_conflict = "rebase-conflict",
    rebase_hooks = "rebase-hooks",
    rebase_exec = "rebase-exec",
    rebase_autostash = "rebase-autostash",
    rebase_i_basic = "rebase-i-basic",
    rebase_i_edit = "rebase-i-edit",
    rebase_i_squash = "rebase-i-squash",
    rebase_i_reword = "rebase-i-reword",
    rebase_i_conflict = "rebase-i-conflict",
    rebase_i_errors = "rebase-i-errors",
    rebase_i_update_refs = "rebase-i-update-refs",
    rebase_i_merges = "rebase-i-merges",
    rebase_i_hooks = "rebase-i-hooks",
    rebase_i_empty = "rebase-i-empty",
    revert_basic = "revert-basic",
    revert_merge = "revert-merge",
    blame_basic = "blame-basic",
    blame_history = "blame-history",
    blame_dirty = "blame-dirty",
    blame_json = "blame-json",
    hooks_husky_pre_commit = "hooks-husky-pre-commit",
    hooks_husky_pre_push = "hooks-husky-pre-push",
    hooks_commitlint = "hooks-commitlint",
    hooks_lint_staged_partial = "lint-staged-partial",
    hooks_lint_staged_binary = "lint-staged-binary",
    hooks_claude_code_flow = "claude-code-flow",
    hooks_path = "hooks-path",
    hooks_reference_transaction = "hooks-reference-transaction",
    worktree_basic = "worktree-basic",
    worktree_admin = "worktree-admin",
    worktree_hook = "worktree-hook",
    worktree_nested = "worktree-nested",
    worktree_claude_hook = "worktree-claude-hook",
    worktree_track = "worktree-track",
    stash_basic = "stash-basic",
    stash_untracked = "stash-untracked",
    stash_paths = "stash-paths",
    stash_conflict = "stash-conflict",
    stash_plumbing = "stash-plumbing",
    remote_bucket = "remote-bucket",
    push_reject = "push-reject",
    fetch_agent_refs = "fetch-agent-refs",
    git_remote_roundtrip = "git-remote-roundtrip",
    apply_basic = "apply-basic",
    apply_offset = "apply-offset",
    apply_errors = "apply-errors",
    apply_rename = "apply-rename",
    apply_unsafe_paths = "apply-unsafe-paths",
    apply_unsafe_asc = "apply-unsafe-asc",
    format_patch_basic = "format-patch-basic",
    format_patch_files = "format-patch-files",
    format_patch_text = "format-patch-text",
    am_basic = "am-basic",
    am_conflict = "am-conflict",
    am_hooks = "am-hooks",
    bisect_basic = "bisect-basic",
    bisect_skip = "bisect-skip",
    bisect_run = "bisect-run",
    bisect_merge = "bisect-merge",
    plumbing_write_commit_tree = "plumbing-write-commit-tree",
    plumbing_write_mktree = "plumbing-write-mktree",
    plumbing_write_merge_tree = "plumbing-write-merge-tree",
    plumbing_write_trailers = "plumbing-write-trailers",
    shortlog_basic = "shortlog-basic",
    name_rev_basic = "name-rev-basic",
    range_diff_basic = "range-diff-basic",
    cherry_basic = "cherry-basic",
    notes_show = "notes-show",
    stack_invisible = "stack-invisible",
    rev_parse_unknown_option = "rev-parse-unknown-option",
    branchless_push_tag = "branchless-push-tag",
    branchless_push_all = "branchless-push-all",
    branchless_push = "branchless-push",
}

#[test]
fn every_scenario_has_a_test() {
    let names = tools().scenario_names();
    let missing: Vec<_> = names
        .iter()
        .filter(|n| !REGISTERED.contains(&n.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "scenarios without a test in compat.rs: {missing:?}"
    );
    let stale: Vec<_> = REGISTERED
        .iter()
        .filter(|n| !names.iter().any(|m| m == *n))
        .collect();
    assert!(stale.is_empty(), "tests without a scenario file: {stale:?}");
}

#[test]
fn allowed_toml_parses_and_every_entry_is_justified() {
    tools().allowed().expect("allowed.toml");
}

/// Self-check (SPEC §17.1): a deliberate fall-through to real git fails the runner. `config
/// --global` is repository-independent, so the shim hands it to real git.
#[test]
fn a_fall_through_on_the_asc_side_fails_the_case() {
    let err = tools()
        .check_script(
            "selfcheck-fall-through",
            "g init\ng config --global compat.selfcheck 1\n",
        )
        .expect_err("the runner must reject a fall-through");
    assert!(err.contains("fell through to real git"), "{err}");
}

/// Self-check: a difference between the sides fails the case, and an `allowed.toml` entry for it
/// lets the case pass. `ASC_HOME` exists only on the asc side.
#[test]
fn a_difference_between_the_sides_fails_the_case() {
    let script = "g init\nif [ -n \"${ASC_HOME:-}\" ]; then echo asc-side; else echo git-side; fi\n";
    let err = tools()
        .check_script("selfcheck-diff", script)
        .expect_err("the sides differ");
    assert!(
        err.contains("--- stdout") && err.contains("-git-side") && err.contains("+asc-side"),
        "{err}"
    );
}

#[test]
fn allowed_entries_drop_lines_from_both_sides() {
    use harness::{Allow, Capture, compare};
    let side = |line: &str| {
        let mut c = Capture::default();
        c.sections
            .insert("stdout".into(), format!("same\n{line}\n").into_bytes());
        c
    };
    let allow = |scenario: &str| Allow {
        scenario: scenario.into(),
        section: "stdout".into(),
        drop: "^(git|asc)-only$".into(),
        reason: "test".into(),
        spec: "§17.1".into(),
    };
    assert!(compare("c", &side("git-only"), &side("asc-only"), &[allow("c")]).is_ok());
    assert!(compare("c", &side("git-only"), &side("asc-only"), &[allow("other")]).is_err());
}

// ---- the preflight and the double run (PLAN Q03; adapter gaps A-1, A-2, A-3, A-5, A-6) --------

use harness::preflight as pf;
use harness::{Probe, Ran, Side, Verdict};

/// A probe that answers every check as a healthy machine does, except where a test forces one.
struct Fake {
    exec_bit: bool,
    shell: bool,
    oracle: Result<String, String>,
}

impl Fake {
    fn healthy() -> Self {
        Fake {
            exec_bit: true,
            shell: true,
            oracle: Ok("the pinned build".into()),
        }
    }
}

impl Probe for Fake {
    fn run(&self, _side: Side, script: &str) -> Result<Ran, String> {
        let said = |s: &str| {
            Ok(Ran {
                stdout: s.into(),
                code: 0,
            })
        };
        if script == pf::EXEC_BIT_SCRIPT {
            // A file system that drops the exec bit: `./probe.sh` is "Permission denied" (126).
            return if self.exec_bit {
                said("ran\n")
            } else {
                Ok(Ran {
                    stdout: String::new(),
                    code: 126,
                })
            };
        }
        if script == pf::SHELL_SCRIPT {
            // A shell without `$(( ))` prints the expression back.
            return if self.shell {
                said(&format!("\n{}", pf::SHELL_GOLDEN))
            } else {
                said("\narith=$((2 + 3 * 4))\n")
            };
        }
        match script {
            s if s == pf::DEFAULT_BRANCH_SCRIPT => said(pf::BRANCH_GOLDEN),
            s if s == pf::TIME_ZONE_SCRIPT => said(pf::TIME_ZONE_GOLDEN),
            s if s == pf::LOCALE_SCRIPT => said(pf::LOCALE_GOLDEN),
            other => Err(format!("unexpected probe script {other:?}")),
        }
    }

    fn oracle(&self) -> Result<String, String> {
        self.oracle.clone()
    }
}

fn failed(verdicts: &[Verdict]) -> Vec<&Verdict> {
    verdicts.iter().filter(|v| !v.ok).collect()
}

#[test]
fn preflight_passes_every_check_on_a_healthy_machine() {
    let v = pf::run_all(&Fake::healthy());
    let names: Vec<_> = v.iter().map(|v| v.check).collect();
    assert_eq!(
        names,
        [
            "exec-bit",
            "shell",
            "default-branch",
            "time-zone",
            "locale",
            "oracle"
        ]
    );
    assert!(failed(&v).is_empty(), "{v:?}");
}

/// The real machine: if this fails, the environment is not the suite's, and so is every failure.
#[test]
fn preflight_this_machine_is_clean() {
    let bad: Vec<String> = tools()
        .preflight()
        .iter()
        .filter(|v| !v.ok)
        .map(Verdict::line)
        .collect();
    assert!(
        bad.is_empty(),
        "this machine is not the suite's environment:\n{}",
        bad.join("\n")
    );
}

#[test]
fn preflight_a_forced_exec_bit_failure_prints_a6() {
    let v = pf::run_all(&Fake {
        exec_bit: false,
        ..Fake::healthy()
    });
    let bad = failed(&v);
    assert_eq!(bad.len(), 1, "{v:?}");
    assert_eq!((bad[0].check, bad[0].gap), ("exec-bit", "A-6"));
    assert!(bad[0].line().starts_with("adapter gap A-6: exec-bit:"));
    // A difference in any section is explained, and the case says the product was not judged.
    let why = pf::explanation(&v, &["stdout".into()]).expect("explained");
    let text = pf::adapter_failure("case", why, "--- stdout\n");
    assert!(
        text.contains("adapter gap A-6: exec-bit:") && text.contains("the product was not judged"),
        "{text}"
    );
}

#[test]
fn preflight_a_forced_wrong_shell_prints_a5() {
    let v = pf::run_all(&Fake {
        shell: false,
        ..Fake::healthy()
    });
    let bad = failed(&v);
    assert_eq!(bad.len(), 1, "{v:?}");
    assert_eq!((bad[0].check, bad[0].gap), ("shell", "A-5"));
    assert!(bad[0].line().starts_with("adapter gap A-5: shell:"));
    assert!(bad[0].detail.contains("golden output"), "{}", bad[0].detail);
}

#[test]
fn preflight_a_forced_wrong_oracle_prints_a3_or_a4() {
    for (message, gap) in [
        (
            "adapter gap A-3: the oracle /x/git is 'git version 2.47.3', the pin is 2.55.0",
            "A-3",
        ),
        (
            "adapter gap A-4: /x/git has sha256 aa, the record says bb",
            "A-4",
        ),
    ] {
        let v = pf::run_all(&Fake {
            oracle: Err(message.into()),
            ..Fake::healthy()
        });
        let bad = failed(&v);
        assert_eq!(bad.len(), 1, "{v:?}");
        assert_eq!((bad[0].check, bad[0].gap), ("oracle", gap));
        assert!(
            bad[0]
                .line()
                .starts_with(&format!("adapter gap {gap}: oracle: ")),
            "{}",
            bad[0].line()
        );
    }
}

#[test]
fn preflight_a_locale_failure_explains_only_the_message_streams() {
    assert!(pf::explains("locale", "stdout") && pf::explains("locale", "stderr"));
    assert!(!pf::explains("locale", "post:for-each-ref") && !pf::explains("locale", "exit"));
    assert!(pf::explains("exec-bit", "post:tree/a.txt"));
    let v = [Verdict {
        check: "locale",
        ok: false,
        detail: "x".into(),
        gap: "A-6",
    }];
    assert!(pf::explanation(&v, &["post:status".into()]).is_none());
    assert!(pf::explanation(&v, &["post:status".into(), "stderr".into()]).is_some());
}

/// A real difference with a clean preflight is still a product failure, reported as before.
#[test]
fn preflight_a_real_difference_with_a_clean_preflight_is_a_product_failure() {
    assert!(failed(tools().preflight()).is_empty());
    let script = "g init\nif [ -n \"${ASC_HOME:-}\" ]; then echo asc-side; else echo git-side; fi\n";
    let err = tools()
        .check_script("selfcheck-product", script)
        .expect_err("the sides differ");
    assert!(
        err.contains("asc differs from git") && err.contains("+asc-side"),
        "{err}"
    );
    assert!(!err.contains("adapter gap"), "{err}");
}

#[test]
fn preflight_program_name_in_hints_is_normalised() {
    let n = harness::Normaliser::new(std::path::Path::new("/nonexistent-compat-root"));
    let apply = |s: &str| String::from_utf8(n.apply(s.as_bytes())).expect("utf8");
    assert_eq!(
        apply("hint: use 'asc pull' before pushing\n"),
        "hint: use 'git pull' before pushing\n"
    );
    assert_eq!(
        apply("usage: asc status [<options>]\n   or: asc status -h\n"),
        "usage: git status [<options>]\n   or: git status -h\n"
    );
    // Only the program word, and only in hint and usage text.
    for same in [
        "hint: asc-only and asc: tags stay, as does ./asc\n",
        "fatal: asc: offline (origin unreachable)\n",
        "error: run asc commit again\n",
        "hint: use 'git pull' before pushing\n",
    ] {
        assert_eq!(apply(same), same);
    }
}

#[test]
fn preflight_the_branchless_marker_is_read_from_the_first_five_lines() {
    let both = vec![Some(true), Some(false)];
    assert_eq!(
        harness::branchless_modes("# compat: branchless=both\nx\n"),
        both
    );
    assert_eq!(
        harness::branchless_modes("a\nb\nc\nd\n# compat: branchless=both\n"),
        both
    );
    for single in [
        "a\nb\nc\nd\ne\n# compat: branchless=both\n",
        "g init\n",
        "# compat: branchless=true\n",
    ] {
        assert_eq!(harness::branchless_modes(single), vec![None], "{single}");
    }
}

#[test]
fn preflight_the_double_run_sets_both_modes_and_compares_them() {
    // Local commands are the same in both modes, and the same as git's.
    tools()
        .check_script(
            "selfcheck-both-same",
            "# compat: branchless=both\ng init\necho x > f\ng add f\nat 1\ng commit -m one\ng log\n",
        )
        .expect("identical in both modes");
    // A mode-dependent result fails: the asc side sees ASC_BRANCHLESS, git does not.
    let script = "# compat: branchless=both\ng init\nif [ \"${ASC_BRANCHLESS:-}\" = false ]; then echo off; else echo on; fi\n";
    let err = tools()
        .check_script("selfcheck-both-differ", script)
        .expect_err("the modes differ");
    assert!(
        err.contains("asc.branchless=true and asc.branchless=false")
            && err.contains("-on")
            && err.contains("+off"),
        "{err}"
    );
    assert!(
        err.contains("asc (asc.branchless=false) differs from git"),
        "{err}"
    );
    assert!(
        !err.contains("asc (asc.branchless=true) differs from git"),
        "{err}"
    );
}
