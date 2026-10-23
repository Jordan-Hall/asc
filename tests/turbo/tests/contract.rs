//! U61p acceptance: turbo 2.10.11 against a plain asc workspace (SPEC section 17, "Turbo contract").
//! Run with `cargo test -p asc-turbo-contract plain -- --ignored` (needs bun and network for
//! `bunx turbo@2.10.11`). Each case builds the same fixture as a real git clone and as a plain
//! asc workspace with the shim first on the PATH, puts both in the same state and compares turbo's
//! answers.
//!
//! A plain workspace has no virtual file system, so it hydrates nothing: the "0 hydrations beyond
//! manifests" counters belong to the virtual suite (U61v).

use std::fs;

use asc_turbo_contract::{
    Lab, PACKAGES, Side, affected, dry_hashes, edit_package, package_dir, package_name,
};

/// Both sides, built from the fixture in one commit.
fn pair(lab: &Lab) -> (std::path::PathBuf, std::path::PathBuf) {
    (lab.repo(Side::Git, "clone"), lab.repo(Side::Asc, "ws"))
}

fn assert_same(
    git: &std::collections::BTreeMap<String, String>,
    asc: &std::collections::BTreeMap<String, String>,
) {
    assert_eq!(git.len(), PACKAGES, "one build task per package: {git:?}");
    assert_eq!(
        git, asc,
        "task hashes differ between the git clone and the asc workspace"
    );
}

#[test]
#[ignore = "needs bun and network"]
fn plain_dry_run_hashes_equal_a_git_clone() {
    let lab = Lab::new();
    let (git, asc) = pair(&lab);
    let (want, got) = (
        dry_hashes(&lab, Side::Git, &git),
        dry_hashes(&lab, Side::Asc, &asc),
    );
    assert_same(&want, &got);
}

#[test]
#[ignore = "needs bun and network"]
fn plain_one_file_edit_changes_only_its_tasks() {
    let lab = Lab::new();
    let (git, asc) = pair(&lab);
    let before = dry_hashes(&lab, Side::Asc, &asc);
    for root in [&git, &asc] {
        edit_package(root, 5, "export const id = 'edited';\n");
    }
    let (want, got) = (
        dry_hashes(&lab, Side::Git, &git),
        dry_hashes(&lab, Side::Asc, &asc),
    );
    assert_same(&want, &got);
    // Package 5 and its dependents (10, 11) change; nothing else does.
    let changed: Vec<&str> = got
        .iter()
        .filter(|(task, hash)| before[*task] != **hash)
        .map(|(task, _)| task.as_str())
        .collect();
    let expect: Vec<String> = [5, 10, 11]
        .iter()
        .map(|i| format!("{}#build", package_name(*i)))
        .collect();
    let mut changed_sorted = changed.clone();
    changed_sorted.sort_unstable();
    let mut expect_sorted: Vec<&str> = expect.iter().map(String::as_str).collect();
    expect_sorted.sort_unstable();
    assert_eq!(
        changed_sorted, expect_sorted,
        "only the edit and its dependents change"
    );
}

#[test]
#[ignore = "needs bun and network"]
fn plain_ls_affected_equals_a_git_clone() {
    let lab = Lab::new();
    let (git, asc) = pair(&lab);
    for (side, root) in [(Side::Git, &git), (Side::Asc, &asc)] {
        edit_package(root, 7, "export const id = 'edited';\n");
        lab.commit_all(side, root, "edit p07");
    }
    let want = affected(&lab, Side::Git, &git, "HEAD~1");
    let got = affected(&lab, Side::Asc, &asc, "HEAD~1");
    assert!(
        want.contains(&package_name(7)),
        "the edited package is affected: {want:?}"
    );
    assert_eq!(want, got, "turbo ls --affected differs");
}

#[test]
#[ignore = "needs bun and network"]
fn plain_racy_run_after_checkout_verifies_no_entries() {
    let lab = Lab::new();
    let asc = lab.repo(Side::Asc, "ws");
    // Rewrite a file by switching branches, then run turbo at once: every index entry must be
    // newer than nothing, so none is racy and turbo has no entry left to verify by hashing. Turbo's
    // trace log states, per package, how many files it had to hash (`to_hash_count`).
    lab.shim(&asc, &["checkout", "-q", "-b", "other"])
        .ok("checkout -b");
    edit_package(
        &asc,
        7,
        "export const id = 'other';
",
    );
    lab.commit_all(Side::Asc, &asc, "other");
    lab.shim(&asc, &["checkout", "-q", "master"])
        .ok("checkout master");
    let run = lab
        .turbo(
            Side::Asc,
            &asc,
            &["run", "build", "--dry=json", "-vvv"],
            &[("TURBO_LOG_VERBOSITY", "trace")],
        )
        .ok("turbo");
    let counts: Vec<u64> = run
        .stderr
        .lines()
        .filter(|l| l.contains("filtered repo index for package"))
        .filter_map(|l| l.rsplit("to_hash_count=").next())
        .filter_map(|n| n.trim().parse().ok())
        .collect();
    assert_eq!(
        counts.len(),
        PACKAGES,
        "one repo-index line per package in turbo's trace log:
{}",
        run.stderr
    );
    assert!(
        counts.iter().all(|n| *n == 0),
        "racy entries had to be verified by hashing: {counts:?}
{}",
        run.stderr
            .lines()
            .filter(|l| l.contains("filtered repo index") && !l.contains("to_hash_count=0"))
            .collect::<Vec<_>>()
            .join(
                "
"
            )
    );
}

#[test]
#[ignore = "needs bun and network"]
fn plain_worktrees_share_the_repo_turbo_cache() {
    let lab = Lab::new();
    let asc = lab.repo(Side::Asc, "ws");
    lab.shim(&asc, &["worktree", "add", "../second", "-b", "other"])
        .ok("worktree add");
    let second = asc.parent().expect("parent").join("second");
    let first = lab
        .turbo(Side::Asc, &asc, &["run", "build"], &[])
        .ok("first run");
    assert!(first.stdout.contains("cache miss"), "{}", first.stdout);
    let again = lab
        .turbo(Side::Asc, &second, &["run", "build"], &[])
        .ok("second worktree run");
    assert!(
        again.stdout.contains("FULL TURBO"),
        "the second worktree hits the first one's cache:\n{}",
        again.stdout
    );
    let caches: Vec<_> = fs::read_dir(lab.asc_home.join("repos"))
        .expect("repos dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(".turbo/cache").is_dir())
        .collect();
    assert_eq!(
        caches.len(),
        1,
        "one shared cache under repos/<id>/.turbo/cache"
    );
    let _ = package_dir(0);
}

/// Stages one kind of change in both repositories and compares the dry-run hashes.
fn staged_case(change: impl Fn(&Lab, Side, &std::path::Path)) {
    let lab = Lab::new();
    let (git, asc) = pair(&lab);
    change(&lab, Side::Git, &git);
    change(&lab, Side::Asc, &asc);
    let (want, got) = (
        dry_hashes(&lab, Side::Git, &git),
        dry_hashes(&lab, Side::Asc, &asc),
    );
    assert_same(&want, &got);
}

#[test]
#[ignore = "needs bun and network"]
fn plain_staged_new_file_hashes_like_a_git_clone() {
    staged_case(|lab, side, root| {
        let path = root.join(package_dir(3)).join("src/added.js");
        fs::write(path, "export const added = true;\n").expect("write");
        lab.git(side, root, &["add", "packages/p03/src/added.js"])
            .ok("add");
    });
}

#[test]
#[ignore = "needs bun and network"]
fn plain_staged_delete_hashes_like_a_git_clone() {
    staged_case(|lab, side, root| {
        lab.git(side, root, &["rm", "-q", "packages/p04/src/index.js"])
            .ok("rm");
    });
}

#[test]
#[ignore = "needs bun and network"]
fn plain_staged_modify_hashes_like_a_git_clone() {
    staged_case(|lab, side, root| {
        edit_package(root, 6, "export const id = 'staged';\n");
        lab.git(side, root, &["add", "packages/p06/src/index.js"])
            .ok("add");
    });
}
