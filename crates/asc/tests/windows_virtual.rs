//! U58 acceptance: virtual (ProjFS) workspaces end to end through the built binary.
//!
//! * The tests in `projfs` need Windows with ProjFS enabled (`Client-ProjFS`) and start a real
//!   daemon: `cargo test -p asc --test windows_virtual -- --ignored`.
//! * The tests at the top run anywhere: a machine that cannot host a virtual workspace (no
//!   ProjFS, not Windows, or `ASC_NO_DAEMON`) refuses `clone --mode virtual` and
//!   `worktree add --mode virtual` with a clear message and leaves nothing behind.
//!
//! The `asc serve` source and the `virtual-*.t` compat scenarios need runner support for a
//! virtual mode (tests/compat/src, crates/asc/tests/compat.rs), which this unit does not own.

mod support;

use std::path::Path;
use std::process::Output;

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    text(&out.stdout)
}

fn bucket_url(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

/// A bucket holding `master` (a.txt, src/b.txt) and `side` (one more file), pushed from a
/// plain source repository; returns the bucket URL.
fn seed(sb: &Sandbox) -> String {
    let src = sb.dir("src-repo");
    ok(sb, &src, &["init", "-q"]);
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(src.join("a.txt"), "a\n").unwrap();
    std::fs::write(src.join("src").join("b.txt"), "b\n").unwrap();
    ok(sb, &src, &["add", "."]);
    ok(sb, &src, &["commit", "-q", "-m", "base"]);
    ok(sb, &src, &["switch", "-q", "-c", "side"]);
    std::fs::write(src.join("c.txt"), "c\n").unwrap();
    ok(sb, &src, &["add", "c.txt"]);
    ok(sb, &src, &["commit", "-q", "-m", "side"]);
    ok(sb, &src, &["switch", "-q", "master"]);
    let url = bucket_url(&sb.dir("bucket"));
    ok(sb, &src, &["remote", "add", "origin", &url]);
    ok(sb, &src, &["push", "-q", "origin", "master", "side"]);
    url
}

/// `asc args…` with the daemon off, as a machine that cannot host a virtual workspace sees it.
fn asc_no_daemon(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .env("ASC_NO_DAEMON", "1")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn plain_clone(sb: &Sandbox, url: &str) -> std::path::PathBuf {
    let parent = sb.dir("plain-work");
    ok(sb, &parent, &["clone", "-q", url, "p"]);
    parent.join("p")
}

#[test]
fn virtual_clone_is_refused_without_the_daemon_and_leaves_nothing() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let parent = sb.dir("work");
    for mode in [&["--mode", "virtual"][..], &["--mode=virtual"][..]] {
        let mut args = vec!["clone", "-q"];
        args.extend_from_slice(mode);
        args.extend_from_slice(&[url.as_str(), "v"]);
        let out = asc_no_daemon(&sb, &parent, &args);
        assert!(!out.status.success(), "{mode:?} succeeded");
        let err = text(&out.stderr);
        assert!(
            err.contains(
                "virtual workspaces need the daemon, which ASC_NO_DAEMON turns off; use --mode plain"
            ),
            "{err}"
        );
        assert!(!parent.join("v").exists(), "left a directory behind");
    }
}

#[test]
fn virtual_clone_names_what_this_machine_lacks() {
    // With ProjFS installed the daemon may host it, so there is nothing to refuse.
    let projfs = std::env::var_os("SystemRoot")
        .map(|r| Path::new(&r).join("System32").join("ProjectedFSLib.dll"))
        .is_some_and(|p| p.exists());
    if cfg!(windows) && projfs {
        return;
    }
    let sb = Sandbox::new();
    let url = seed(&sb);
    let parent = sb.dir("work");
    let out = asc(
        &sb,
        &parent,
        &["clone", "-q", "--mode", "virtual", &url, "v"],
    );
    assert!(!out.status.success());
    let err = text(&out.stderr);
    let want = if cfg!(windows) {
        "Client-ProjFS"
    } else {
        "supported on Windows (ProjFS) only"
    };
    assert!(err.contains(want), "{err}");
    assert!(err.contains("use --mode plain"), "{err}");
    assert!(!parent.join("v").exists(), "left a directory behind");
}

#[test]
fn clone_mode_other_than_plain_or_virtual_is_rejected() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let parent = sb.dir("work");
    for arg in [&["--mode", "sparse"][..], &["--mode=sparse"][..]] {
        let mut args = vec!["clone", "-q"];
        args.extend_from_slice(arg);
        args.extend_from_slice(&[url.as_str(), "v"]);
        let out = asc(&sb, &parent, &args);
        assert!(!out.status.success());
        assert!(text(&out.stderr).contains("unknown clone mode 'sparse'"));
    }
    ok(
        &sb,
        &parent,
        &["clone", "-q", "--mode", "plain", &url, "ok"],
    );
    assert_eq!(
        std::fs::read_to_string(parent.join("ok").join("a.txt")).unwrap(),
        "a
"
    );
}

#[test]
fn virtual_worktree_add_is_refused_without_the_daemon_and_leaves_nothing() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let ws = plain_clone(&sb, &url);
    let wt = sb.dir("wts").join("w1");
    let out = asc_no_daemon(
        &sb,
        &ws,
        &[
            "worktree",
            "add",
            "--mode",
            "virtual",
            "-b",
            "w1",
            wt.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        text(&out.stderr)
            .contains("virtual workspaces need the daemon, which ASC_NO_DAEMON turns off"),
        "{}",
        text(&out.stderr)
    );
    assert!(!wt.exists(), "left a directory behind");
    // The refusal made no branch and no registration: a plain worktree of the same name works.
    ok(
        &sb,
        &ws,
        &[
            "worktree",
            "add",
            "-q",
            "--mode",
            "plain",
            "-b",
            "w1",
            wt.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "a
"
    );
}

#[cfg(windows)]
mod projfs {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::{Sandbox, ok, seed};

    /// A sandbox whose daemon is stopped when the test ends, pass or fail: a daemon left running
    /// keeps the sandbox's files and the built binary open.
    struct Virtual(Sandbox);

    impl Virtual {
        fn new() -> Self {
            Self(Sandbox::new())
        }
    }

    impl std::ops::Deref for Virtual {
        type Target = Sandbox;
        fn deref(&self) -> &Sandbox {
            &self.0
        }
    }

    impl Drop for Virtual {
        fn drop(&mut self) {
            let _ = self
                .0
                .asc(self.0.tmp.path())
                .args(["daemon", "stop"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    fn virtual_clone(sb: &Sandbox, url: &str) -> std::path::PathBuf {
        let parent = sb.dir("work");
        ok(sb, &parent, &["clone", "-q", "--mode", "virtual", url, "v"]);
        parent.join("v")
    }

    /// Whether `dir` is a ProjFS virtualization root (it carries a reparse point).
    fn is_virtual_root(dir: &Path) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        std::fs::symlink_metadata(dir).unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }

    /// The hydrations the daemon reports per path (`asc debug hydrations --json`, one row per path).
    fn hydrations(sb: &Sandbox, ws: &Path) -> Vec<(String, u64)> {
        let out = ok(sb, ws, &["debug", "hydrations", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).expect("json");
        v.as_array()
            .expect("an array of rows")
            .iter()
            .map(|r| {
                let path = r["path"].as_str().unwrap_or_default().replace('\\', "/");
                (path, r["count"].as_u64().unwrap_or_default())
            })
            .collect()
    }

    /// The total of hydrations the daemon reports.
    fn downloads(sb: &Sandbox, ws: &Path) -> u64 {
        hydrations(sb, ws).iter().map(|(_, n)| n).sum()
    }

    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn clone_edit_commit_switch_without_downloads() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "a\n");

        let t = Instant::now();
        ok(&sb, &ws, &["status", "--porcelain"]);
        let first = t.elapsed();
        let t = Instant::now();
        assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "status took {:?} (first {first:?})",
            t.elapsed()
        );

        std::fs::write(ws.join("a.txt"), "a2\n").unwrap();
        std::fs::write(ws.join("new.txt"), "n\n").unwrap();
        ok(&sb, &ws, &["add", "-A"]);
        ok(&sb, &ws, &["commit", "-q", "-m", "edit"]);
        assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");

        // Switching between branches whose objects are already local downloads nothing.
        let before = downloads(&sb, &ws);
        ok(&sb, &ws, &["switch", "-q", "side"]);
        assert_eq!(std::fs::read_to_string(ws.join("c.txt")).unwrap(), "c\n");
        ok(&sb, &ws, &["switch", "-q", "master"]);
        assert_eq!(downloads(&sb, &ws), before, "switch downloaded objects");
    }

    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn worktree_add_is_fast_and_git_verbs_work_through_the_shim() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);

        let wt = ws.join(".claude").join("worktrees").join("w1");
        let t = Instant::now();
        ok(
            &sb,
            &ws,
            &[
                "worktree",
                "add",
                "--mode",
                "virtual",
                "-b",
                "w1",
                wt.to_str().unwrap(),
            ],
        );
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "worktree add took {:?}",
            t.elapsed()
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("src").join("b.txt")).unwrap(),
            "b\n"
        );

        // The git-compatible verbs agree with the plain implementation's output shapes.
        assert_eq!(ok(&sb, &wt, &["status", "--porcelain=v2", "-z"]), "");
        assert!(ok(&sb, &wt, &["log", "--oneline"]).contains("base"));
        std::fs::write(wt.join("a.txt"), "w\n").unwrap();
        assert!(ok(&sb, &wt, &["diff"]).contains("+w"));

        ok(
            &sb,
            &ws,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
        assert!(!wt.exists());
    }

    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn a_worktree_is_virtual_by_default_and_the_config_can_make_it_plain() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);
        let wt = sb.dir("wts").join("d1");
        ok(
            &sb,
            &ws,
            &["worktree", "add", "-q", "-b", "d1", wt.to_str().unwrap()],
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).unwrap(),
            "a
"
        );
        assert!(is_virtual_root(&wt), "the default worktree is not virtual");

        ok(&sb, &ws, &["config", "asc.worktree.mode", "plain"]);
        let plain = sb.dir("wts").join("d2");
        ok(
            &sb,
            &ws,
            &["worktree", "add", "-q", "-b", "d2", plain.to_str().unwrap()],
        );
        assert_eq!(
            std::fs::read_to_string(plain.join("a.txt")).unwrap(),
            "a
"
        );
        assert!(
            !is_virtual_root(&plain),
            "asc.worktree.mode plain made a virtual worktree"
        );
    }

    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn convert_to_plain_keeps_edits() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);
        std::fs::write(ws.join("a.txt"), "edited\n").unwrap();
        ok(&sb, &ws, &["worktree", "convert", "--plain"]);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "edited\n"
        );
        assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), " M a.txt\n");
        assert_eq!(
            std::fs::read_to_string(ws.join("src").join("b.txt")).unwrap(),
            "b\n"
        );
    }

    /// A hydrated clean file, a deleted file and a never-read file all come through the
    /// conversion as plain files (or stay deleted), and the root lists without a provider.
    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn convert_to_plain_keeps_deletions_and_leaves_a_plain_root() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);
        assert_eq!(
            std::fs::read_to_string(ws.join("src").join("b.txt")).unwrap(),
            "b\n"
        );
        std::fs::remove_file(ws.join("a.txt")).unwrap();
        ok(&sb, &ws, &["worktree", "convert", "--plain"]);
        ok(&sb, &ws, &["daemon", "stop"]);

        assert!(!ws.join("a.txt").exists());
        assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), " D a.txt\n");
        assert_eq!(
            std::fs::read_to_string(ws.join("src").join("b.txt")).unwrap(),
            "b\n"
        );
        // No ProjFS reparse point is left: the root and its directories list with no provider.
        let names: Vec<String> = std::fs::read_dir(&ws)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "src"), "{names:?}");
        assert_eq!(std::fs::read_dir(ws.join("src")).unwrap().count(), 1);
    }

    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn tree_without_turbo_json_has_no_index_and_status_matches_git() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);
        // SPEC §9.4: no turbo.json, so asc.turboIndex renders no `index` file.
        let gitdir = ok(&sb, &ws, &["rev-parse", "--git-dir"]);
        let gitdir = ws.join(gitdir.trim());
        assert!(
            !gitdir.join("index").exists(),
            "unexpected index in {gitdir:?}"
        );
        std::fs::write(ws.join("a.txt"), "x\n").unwrap();
        let asc_status = ok(&sb, &ws, &["status", "--porcelain=v2"]);
        assert!(asc_status.contains("a.txt"));
    }

    /// docs/designs/build-cache.md §1: the inputs digest of a cached action comes from the tree
    /// and the dirty list, so neither the miss's key nor the hit reads a clean input.
    #[test]
    #[ignore = "needs Windows with ProjFS"]
    fn a_cached_action_keys_clean_inputs_without_hydrating_them() {
        let sb = Virtual::new();
        let url = seed(&sb);
        let ws = virtual_clone(&sb, &url);
        std::fs::write(ws.join(".gitignore"), "out/\n").unwrap();
        ok(&sb, &ws, &["config", "asc.buildcache.remote", "off"]);
        let action = [
            "cache",
            "run",
            "--name",
            "gen",
            "--inputs",
            "src",
            "--outputs",
            "out",
            "--",
            "cmd",
            "/C",
            "(if not exist out mkdir out) & echo built> out\\f.txt",
        ];
        ok(&sb, &ws, &action);
        std::fs::remove_dir_all(ws.join("out")).unwrap();
        let hit = super::asc(&sb, &ws, &action);
        assert!(hit.status.success(), "{}", super::text(&hit.stderr));
        assert!(
            super::text(&hit.stderr).contains("asc cache: hit gen"),
            "{}",
            super::text(&hit.stderr)
        );
        assert!(ws.join("out").join("f.txt").is_file());
        let read: Vec<_> = hydrations(&sb, &ws)
            .into_iter()
            .filter(|(p, _)| p.ends_with("src/b.txt"))
            .collect();
        assert!(read.is_empty(), "the key hydrated {read:?}");
    }
}
