//! U61v acceptance: turbo 2.10.11 against a virtual asc workspace (SPEC section 17, "Turbo
//! contract"). Run with `cargo test -p asc-turbo-contract virtual -- --ignored` (needs bun, network
//! for `bunx turbo@2.10.11`, and ProjFS on Windows; FUSE hosts opt in with `ASC_TURBO_VIRTUAL=1`
//! once U59 lands).
//!
//! These are the U61p cases (`contract.rs`) run in a virtual workspace, plus the hydration
//! counters: after turbo has run, every hydrated path is a manifest. The fixture has a
//! `turbo.json`, so the synthetic index exists; a second fixture without it has no `index`.
//!
//! TODO(U58-wiring): `asc clone --mode virtual` and `asc worktree add --mode virtual` do not reach
//! the ProjFS provider yet (see `crates/asc/tests/windows_virtual.rs`), so these cases fail until
//! that wiring lands.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use asc_turbo_contract::{
    Lab, PACKAGES, Side, affected, dry_hashes, edit_package, package_dir, package_name,
    write_fixture,
};

/// Whether this host can mount a virtual workspace.
fn can_mount() -> bool {
    cfg!(windows) || std::env::var_os("ASC_TURBO_VIRTUAL").is_some()
}

/// The `asc` binary: `ASC_BIN`, else `asc` in the profile directory above this test's `deps`.
fn asc_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("ASC_BIN") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("test exe");
    let dir = exe.parent().and_then(Path::parent).expect("profile dir");
    let name = if cfg!(windows) { "asc.exe" } else { "asc" };
    let bin = dir.join(name);
    assert!(
        bin.is_file(),
        "no asc binary at {}; run cargo build -p asc or set ASC_BIN",
        bin.display()
    );
    bin
}

/// Runs `asc <args>` in `cwd` inside the lab's sandbox, returning stdout; panics on failure.
fn asc(lab: &Lab, cwd: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new(asc_binary());
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if name.starts_with("GIT_") || name.starts_with("ASC_") {
            cmd.env_remove(&key);
        }
    }
    let out = cmd
        .current_dir(cwd)
        .env("ASC_HOME", &lab.asc_home)
        .env("HOME", &lab.user)
        .env("USERPROFILE", &lab.user)
        .env("XDG_CONFIG_HOME", lab.user.join(".config"))
        .env("APPDATA", lab.user.join("AppData/Roaming"))
        .env("LOCALAPPDATA", lab.user.join("AppData/Local"))
        .env("GIT_CONFIG_GLOBAL", lab.user.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args(args)
        .output()
        .expect("spawn asc");
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

/// A bucket directory as a `file:` URL (the sandbox's only remote).
fn bucket_url(dir: &Path) -> String {
    let abs = fs::canonicalize(dir).expect("bucket path");
    let text = abs.to_string_lossy().replace('\\', "/");
    let text = text.trim_start_matches("//?/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// Pushes the plain source repository `src` (branch `master`) to a fresh bucket and returns its URL.
fn publish(lab: &Lab, src: &Path, name: &str) -> String {
    let url = bucket_url(&lab.dir(name));
    lab.shim(src, &["remote", "add", "origin", &url])
        .ok("remote add");
    lab.shim(src, &["push", "-q", "origin", "master"])
        .ok("push");
    url
}

/// A virtual clone of the fixture (with `turbo.json`) named `name`.
fn virtual_repo(lab: &Lab, name: &str) -> PathBuf {
    let src = lab.repo(Side::Asc, &format!("{name}-src"));
    clone_virtual(lab, &src, name)
}

fn clone_virtual(lab: &Lab, src: &Path, name: &str) -> PathBuf {
    let url = publish(lab, src, &format!("{name}-bucket"));
    let parent = lab.dir(&format!("{name}-parent"));
    asc(
        lab,
        &parent,
        &["clone", "-q", "--mode", "virtual", &url, name],
    );
    parent.join(name)
}

/// The real-git side and the virtual side, both holding the fixture in one commit.
fn pair(lab: &Lab) -> (PathBuf, PathBuf) {
    (lab.repo(Side::Git, "clone"), virtual_repo(lab, "ws"))
}

fn assert_same(git: &BTreeMap<String, String>, asc: &BTreeMap<String, String>) {
    assert_eq!(git.len(), PACKAGES, "one build task per package: {git:?}");
    assert_eq!(
        git, asc,
        "task hashes differ between the git clone and the virtual asc workspace"
    );
}

/// Files turbo may read to plan a run: manifests and configuration, never package sources.
fn is_manifest(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    matches!(
        name,
        "package.json" | "turbo.json" | "bun.lock" | "bun.lockb" | ".gitignore" | ".gitattributes"
    )
}

/// Paths hydrated so far, from `asc debug hydrations --json`.
fn hydrated(lab: &Lab, ws: &Path) -> Vec<String> {
    let out = asc(lab, ws, &["debug", "hydrations", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&out).expect("hydrations json");
    rows.as_array()
        .expect("an array of rows")
        .iter()
        .filter(|r| r["count"].as_u64().unwrap_or(0) > 0)
        .filter_map(|r| r["path"].as_str().map(str::to_owned))
        .collect()
}

fn assert_only_manifests(lab: &Lab, ws: &Path) {
    let others: Vec<String> = hydrated(lab, ws)
        .into_iter()
        .filter(|p| !is_manifest(p))
        .collect();
    assert!(others.is_empty(), "hydrations beyond manifests: {others:?}");
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_dry_run_hashes_equal_a_git_clone() {
    if !can_mount() {
        return;
    }
    let lab = Lab::new();
    let (git, asc_ws) = pair(&lab);
    let (want, got) = (
        dry_hashes(&lab, Side::Git, &git),
        dry_hashes(&lab, Side::Asc, &asc_ws),
    );
    assert_same(&want, &got);
    assert_only_manifests(&lab, &asc_ws);
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_one_file_edit_changes_only_its_tasks() {
    if !can_mount() {
        return;
    }
    let lab = Lab::new();
    let (git, asc_ws) = pair(&lab);
    let before = dry_hashes(&lab, Side::Asc, &asc_ws);
    for root in [&git, &asc_ws] {
        edit_package(root, 5, "export const id = 'edited';\n");
    }
    let (want, got) = (
        dry_hashes(&lab, Side::Git, &git),
        dry_hashes(&lab, Side::Asc, &asc_ws),
    );
    assert_same(&want, &got);
    let mut changed: Vec<&str> = got
        .iter()
        .filter(|(task, hash)| before[*task] != **hash)
        .map(|(task, _)| task.as_str())
        .collect();
    changed.sort_unstable();
    let expect: Vec<String> = [5, 10, 11]
        .iter()
        .map(|i| format!("{}#build", package_name(*i)))
        .collect();
    let mut expect: Vec<&str> = expect.iter().map(String::as_str).collect();
    expect.sort_unstable();
    assert_eq!(changed, expect, "only the edit and its dependents change");
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_ls_affected_equals_a_git_clone() {
    if !can_mount() {
        return;
    }
    let lab = Lab::new();
    let (git, asc_ws) = pair(&lab);
    for (side, root) in [(Side::Git, &git), (Side::Asc, &asc_ws)] {
        edit_package(root, 7, "export const id = 'edited';\n");
        lab.commit_all(side, root, "edit p07");
    }
    let want = affected(&lab, Side::Git, &git, "HEAD~1");
    let got = affected(&lab, Side::Asc, &asc_ws, "HEAD~1");
    assert!(
        want.contains(&package_name(7)),
        "the edited package is affected: {want:?}"
    );
    assert_eq!(want, got, "turbo ls --affected differs");
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_racy_run_after_checkout_verifies_no_entries() {
    if !can_mount() {
        return;
    }
    let lab = Lab::new();
    let ws = virtual_repo(&lab, "ws");
    lab.shim(&ws, &["checkout", "-q", "-b", "other"])
        .ok("checkout -b");
    edit_package(&ws, 7, "export const id = 'other';\n");
    lab.commit_all(Side::Asc, &ws, "other");
    lab.shim(&ws, &["checkout", "-q", "master"])
        .ok("checkout master");
    let run = lab
        .turbo(
            Side::Asc,
            &ws,
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
        "one repo-index line per package in turbo's trace log:\n{}",
        run.stderr
    );
    assert!(
        counts.iter().all(|n| *n == 0),
        "racy entries had to be verified by hashing: {counts:?}"
    );
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_worktrees_share_the_repo_turbo_cache() {
    if !can_mount() {
        return;
    }
    let lab = Lab::new();
    let ws = virtual_repo(&lab, "ws");
    let second = ws.parent().expect("parent").join("second");
    asc(
        &lab,
        &ws,
        &[
            "worktree",
            "add",
            "--mode",
            "virtual",
            "-b",
            "other",
            second.to_str().expect("utf-8 path"),
        ],
    );
    let first = lab
        .turbo(Side::Asc, &ws, &["run", "build"], &[])
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
    let caches = fs::read_dir(lab.asc_home.join("repos"))
        .expect("repos dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(".turbo/cache").is_dir())
        .count();
    assert_eq!(caches, 1, "one shared cache under repos/<id>/.turbo/cache");
}

/// Stages one kind of change in both repositories and compares the dry-run hashes.
fn staged_case(change: impl Fn(&Lab, Side, &Path)) {
    let lab = Lab::new();
    let (git, asc_ws) = pair(&lab);
    change(&lab, Side::Git, &git);
    change(&lab, Side::Asc, &asc_ws);
    let (want, got) = (
        dry_hashes(&lab, Side::Git, &git),
        dry_hashes(&lab, Side::Asc, &asc_ws),
    );
    assert_same(&want, &got);
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_staged_new_file_hashes_like_a_git_clone() {
    if !can_mount() {
        return;
    }
    staged_case(|lab, side, root| {
        let path = root.join(package_dir(3)).join("src/added.js");
        fs::write(path, "export const added = true;\n").expect("write");
        lab.git(side, root, &["add", "packages/p03/src/added.js"])
            .ok("add");
    });
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_staged_delete_hashes_like_a_git_clone() {
    if !can_mount() {
        return;
    }
    staged_case(|lab, side, root| {
        lab.git(side, root, &["rm", "-q", "packages/p04/src/index.js"])
            .ok("rm");
    });
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_staged_modify_hashes_like_a_git_clone() {
    if !can_mount() {
        return;
    }
    staged_case(|lab, side, root| {
        edit_package(root, 6, "export const id = 'staged';\n");
        lab.git(side, root, &["add", "packages/p06/src/index.js"])
            .ok("add");
    });
}

/// The git directory of a workspace, as `rev-parse --git-dir` reports it.
fn git_dir(lab: &Lab, ws: &Path) -> PathBuf {
    let out = asc(lab, ws, &["rev-parse", "--git-dir"]);
    ws.join(out.trim())
}

#[test]
#[ignore = "needs bun, network and a virtual file system"]
fn virtual_index_exists_with_turbo_json_and_not_without() {
    if !can_mount() {
        return;
    }
    let lab = Lab::new();
    let with = virtual_repo(&lab, "with");
    assert!(
        git_dir(&lab, &with).join("index").is_file(),
        "a tree with turbo.json has the synthetic index"
    );

    // The same fixture minus turbo.json: a virtual workspace without it has no `index` file.
    let src = lab.dir("without-src");
    lab.shim(&src, &["init"]).ok("shim init");
    write_fixture(&src);
    fs::remove_file(src.join("turbo.json")).expect("remove turbo.json");
    lab.git(Side::Asc, &src, &["add", "-A"]).ok("add");
    lab.git(Side::Asc, &src, &["commit", "-m", "base"])
        .ok("commit");
    let without = clone_virtual(&lab, &src, "without");
    assert!(
        !git_dir(&lab, &without).join("index").exists(),
        "no turbo.json, so no index file"
    );
}
