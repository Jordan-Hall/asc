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
    rebase_i_basic = "rebase-i-basic",    rebase_i_edit = "rebase-i-edit",    rebase_i_squash = "rebase-i-squash",    rebase_i_reword = "rebase-i-reword",    rebase_i_conflict = "rebase-i-conflict",    rebase_i_errors = "rebase-i-errors",    rebase_i_update_refs = "rebase-i-update-refs",    rebase_i_merges = "rebase-i-merges",    rebase_i_hooks = "rebase-i-hooks",    rebase_i_empty = "rebase-i-empty",
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
}

#[test]
fn every_scenario_has_a_test() {
    let names = tools().scenario_names();
    let missing: Vec<_> = names.iter().filter(|n| !REGISTERED.contains(&n.as_str())).collect();
    assert!(missing.is_empty(), "scenarios without a test in compat.rs: {missing:?}");
    let stale: Vec<_> = REGISTERED.iter().filter(|n| !names.iter().any(|m| m == *n)).collect();
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
        .check_script("selfcheck-fall-through", "g init\ng config --global compat.selfcheck 1\n")
        .expect_err("the runner must reject a fall-through");
    assert!(err.contains("fell through to real git"), "{err}");
}

/// Self-check: a difference between the sides fails the case, and an `allowed.toml` entry for it
/// lets the case pass. `ASC_HOME` exists only on the asc side.
#[test]
fn a_difference_between_the_sides_fails_the_case() {
    let script = "g init\nif [ -n \"${ASC_HOME:-}\" ]; then echo asc-side; else echo git-side; fi\n";
    let err = tools().check_script("selfcheck-diff", script).expect_err("the sides differ");
    assert!(err.contains("--- stdout") && err.contains("-git-side") && err.contains("+asc-side"), "{err}");
}

#[test]
fn allowed_entries_drop_lines_from_both_sides() {
    use harness::{Allow, Capture, compare};
    let side = |line: &str| {
        let mut c = Capture::default();
        c.sections.insert("stdout".into(), format!("same\n{line}\n").into_bytes());
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
