//! `asc worktree add` clones the source worktree's `node_modules` by reflink (`asc.worktree.cloneDeps`,
//! SPEC §9.3.2), through the built binary.
//!
//! The test volume may not reflink (NTFS, ext4): then the command prints the volume hint and
//! clones nothing, and the test checks that instead. Both outcomes keep the files byte-equal or
//! absent; neither ever copies bytes.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
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
        .env("ASC_NO_DAEMON", "1")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
}

fn bucket_url(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

/// A clone holding a package with a lockfile, and an installed `node_modules` beside it.
fn clone(sb: &Sandbox) -> PathBuf {
    let src = sb.dir("src-repo");
    ok(sb, &src, &["init", "-q"]);
    fs::write(src.join("package.json"), "{\"name\":\"app\"}\n").unwrap();
    fs::write(src.join("bun.lock"), "{\"lockfileVersion\":1}\n").unwrap();
    fs::write(src.join(".gitignore"), "node_modules/\n").unwrap();
    ok(sb, &src, &["add", "."]);
    ok(sb, &src, &["commit", "-q", "-m", "base"]);
    let url = bucket_url(&sb.dir("bucket"));
    ok(sb, &src, &["remote", "add", "origin", &url]);
    ok(sb, &src, &["push", "-q", "origin", "master"]);
    let parent = sb.dir("work");
    ok(sb, &parent, &["clone", "-q", &url, "p"]);
    let ws = parent.join("p");
    let pkg = ws.join("node_modules").join("left-pad");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("index.js"), "module.exports = 1;\n").unwrap();
    ws
}

fn add(sb: &Sandbox, ws: &Path, name: &str) -> (PathBuf, String) {
    let wt = sb.tmp.path().join("wts").join(name);
    let out = ok(
        sb,
        ws,
        &["worktree", "add", "-b", name, wt.to_str().unwrap()],
    );
    (wt, text(&out.stderr))
}

#[test]
fn worktree_add_clones_node_modules_or_says_why_not() {
    let sb = Sandbox::new();
    let ws = clone(&sb);
    let (wt, err) = add(&sb, &ws, "w1");
    let got = wt.join("node_modules").join("left-pad").join("index.js");
    if err.contains("Cloned node_modules by reflink") {
        assert_eq!(fs::read_to_string(&got).unwrap(), "module.exports = 1;\n");
        assert!(
            ws.join("node_modules")
                .join("left-pad")
                .join("index.js")
                .exists(),
            "the source keeps its copy"
        );
    } else {
        assert!(
            err.contains("this volume cannot reflink"),
            "neither cloned nor the volume hint: {err}"
        );
        assert!(!wt.join("node_modules").exists(), "no byte copy fallback");
    }
    // No probe or temp directory is left behind on either side.
    for dir in [&ws, &wt] {
        let left: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("asc-clone") || n.contains("asc-reflink"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

#[test]
fn a_changed_lockfile_or_the_setting_off_clones_nothing() {
    let sb = Sandbox::new();
    let ws = clone(&sb);
    // The source's lockfile no longer matches what the new worktree checks out.
    fs::write(ws.join("bun.lock"), "{\"lockfileVersion\":2}\n").unwrap();
    let (wt, err) = add(&sb, &ws, "w1");
    assert!(!wt.join("node_modules").exists());
    assert!(
        err.contains("bun.lock differs") || err.contains("this volume cannot reflink"),
        "{err}"
    );
    fs::write(ws.join("bun.lock"), "{\"lockfileVersion\":1}\n").unwrap();
    ok(&sb, &ws, &["config", "asc.worktree.cloneDeps", "false"]);
    let (wt, err) = add(&sb, &ws, "w2");
    assert!(!wt.join("node_modules").exists());
    assert!(!err.contains("node_modules"), "{err}");
}
