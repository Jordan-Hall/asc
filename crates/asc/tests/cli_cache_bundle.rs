//! `asc cache restore` and `asc cache save`, the two halves a build tool's plugin calls: a miss
//! names the key, `save` stores the outputs under it, and `restore` in another work tree with the
//! same inputs and parts replaces the outputs there. A changed part or input is a miss.

mod support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{Sandbox, asc_binary, text};

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = sb.asc(cwd).args(args).output().expect("spawn asc");
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

/// A work tree with `src/in.txt`; `out/` is ignored, so outputs are never inputs.
fn repo(sb: &Sandbox, name: &str) -> PathBuf {
    let repo = sb.dir(name);
    std::fs::write(repo.join(".gitignore"), "out/\n").unwrap();
    ok(sb, &repo, &["init", "-q"]);
    ok(sb, &repo, &["config", "asc.buildcache.remote", "off"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/in.txt"), "one\n").unwrap();
    repo
}

fn restore(sb: &Sandbox, repo: &Path, mode: &str) -> Value {
    let out = ok(
        sb,
        repo,
        &[
            "cache",
            "restore",
            "--json",
            "--tool",
            "web",
            "--name",
            "app",
            "--inputs",
            "src",
            "--outputs",
            "out",
            "--part",
            &format!("mode={mode}"),
            "--part",
            "version=1",
        ],
    );
    serde_json::from_str(&out).unwrap()
}

fn save(sb: &Sandbox, repo: &Path, key: &str) {
    ok(
        sb,
        repo,
        &[
            "cache",
            "save",
            "--key",
            key,
            "--tool",
            "web",
            "--name",
            "app",
            "--outputs",
            "out",
        ],
    );
}

#[test]
fn a_saved_build_is_restored_in_another_work_tree() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let a = repo(&sb, "a");
    let miss = restore(&sb, &a, "production");
    assert_eq!(miss["hit"], false);
    let key = miss["key"].as_str().unwrap().to_owned();
    std::fs::create_dir_all(a.join("out/assets")).unwrap();
    std::fs::write(a.join("out/index.html"), "<p>one</p>\n").unwrap();
    std::fs::write(a.join("out/assets/app.js"), "one()\n").unwrap();
    save(&sb, &a, &key);

    // The same inputs elsewhere: a hit, and a stale file in the output is gone.
    let b = repo(&sb, "b");
    std::fs::create_dir_all(b.join("out")).unwrap();
    std::fs::write(b.join("out/stale.txt"), "old\n").unwrap();
    let hit = restore(&sb, &b, "production");
    assert_eq!(hit["hit"], true);
    assert_eq!(hit["key"], key.as_str());
    assert_eq!(
        std::fs::read_to_string(b.join("out/assets/app.js")).unwrap(),
        "one()\n"
    );
    assert!(!b.join("out/stale.txt").exists());

    // Another part, and another input, are misses with other keys.
    let mode = restore(&sb, &b, "development");
    assert_eq!(mode["hit"], false);
    assert_ne!(mode["key"], key.as_str());
    std::fs::write(b.join("src/in.txt"), "two\n").unwrap();
    let input = restore(&sb, &b, "production");
    assert_eq!(input["hit"], false);
    assert_ne!(input["key"], key.as_str());

    let ls: Value =
        serde_json::from_str(&ok(&sb, &a, &["cache", "ls", "--tool", "web", "--json"])).unwrap();
    assert_eq!(ls.as_array().unwrap().len(), 1);
}

#[test]
fn a_miss_without_json_exits_one_and_save_needs_an_output() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let a = repo(&sb, "a");
    let out = sb
        .asc(&a)
        .args([
            "cache",
            "restore",
            "--tool",
            "web",
            "--name",
            "app",
            "--inputs",
            "src",
            "--outputs",
            "out",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let key = text(&out.stdout).trim().to_owned();
    assert_eq!(key.len(), 64, "{key}");

    let none = sb
        .asc(&a)
        .args([
            "cache",
            "save",
            "--key",
            &key,
            "--tool",
            "web",
            "--outputs",
            "out",
        ])
        .output()
        .unwrap();
    assert!(!none.status.success());

    let bad = sb
        .asc(&a)
        .args(["cache", "restore", "--tool", "Web!", "--outputs", "out"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
}
