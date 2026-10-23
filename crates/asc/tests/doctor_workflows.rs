//! B24 acceptance: `asc doctor --workflows` lists `on: push` triggers for non-shared branches.

mod support;

use std::fs;

use support::{Sandbox, text};

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn lists_push_triggers_that_name_non_shared_branches() {
    let sb = Sandbox::new();
    let root = sb.dir("repo");
    write(
        &root,
        ".forgejo/workflows/ci.yml",
        "name: ci\non:\n  pull_request:\n  push:\n    branches: [master, 'release/*']\njobs: {}\n",
    );
    write(&root, ".forgejo/workflows/any.yml", "on: push\njobs: {}\n");
    write(
        &root,
        ".github/workflows/feat.yaml",
        "on:\n  push:\n    branches:\n      - master\n      - 'feature/**'\n",
    );
    write(
        &root,
        ".forgejo/workflows/tags.yml",
        "on:\n  push:\n    tags: ['v*']\n",
    );
    let out = sb.run(&root, &["doctor", "--workflows"]);
    let so = text(&out.stdout);
    assert!(out.status.success(), "{so}{}", text(&out.stderr));
    assert!(so.contains("warn  .forgejo/workflows/any.yml"), "{so}");
    assert!(so.contains("no branch filter"), "{so}");
    assert!(so.contains("warn  .github/workflows/feat.yaml"), "{so}");
    assert!(so.contains("feature/**"), "{so}");
    assert!(!so.contains("ci.yml"), "{so}");
    assert!(!so.contains("tags.yml"), "{so}");
}

#[test]
fn clean_repo_reports_ok() {
    let sb = Sandbox::new();
    let root = sb.dir("clean");
    write(
        &root,
        ".forgejo/workflows/ci.yml",
        "on:\n  pull_request:\n  push:\n    branches: [master]\n",
    );
    let out = sb.run(&root, &["doctor", "--workflows"]);
    let so = text(&out.stdout);
    assert!(out.status.success(), "{so}");
    assert!(so.contains("ok    workflows: 1 workflow file(s)"), "{so}");
    assert!(!so.contains("warn"), "{so}");
}

#[test]
fn no_workflow_directory_is_ok() {
    let sb = Sandbox::new();
    let root = sb.dir("empty");
    let out = sb.run(&root, &["doctor", "--workflows"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("0 workflow file(s)"));
}

#[cfg(unix)]
#[test]
fn symlinked_workflow_dir_is_not_followed() {
    let sb = Sandbox::new();
    let root = sb.dir("link");
    let outside = sb.dir("outside");
    write(&outside, "x.yml", "on: push\n");
    fs::create_dir_all(root.join(".forgejo")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join(".forgejo/workflows")).unwrap();
    let out = sb.run(&root, &["doctor", "--workflows"]);
    assert!(!text(&out.stdout).contains("x.yml"));
}
