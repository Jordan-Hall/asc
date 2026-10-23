//! G08 acceptance: `asc affected`, `asc projects` and `asc graph` through the built binary, on
//! repositories the fixture builds directly (no sibling command).

mod support;

use std::fs;
use std::process::Output;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use serde_json::Value;
use support::{Fixture, text};

const A: &str = r#"{"name":"a","version":"1.0.0"}"#;
const B: &str = r#"{"name":"b","version":"1.0.0"}"#;
const ROOT: &str = r#"{"name":"root","private":true,"workspaces":["pkgs/*"]}"#;

async fn set_ref(f: &Fixture, name: &str, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(name.as_bytes().to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "fixture".into(),
            via: "test".into(),
        },
        reason: "fixture".into(),
        op_id: uuid::Uuid::now_v7(),
    };
    Refs::update(&f.db.refs(), f.repo, &txn).await.unwrap();
}

/// Two Node packages under a workspace root; the second commit edits a source file of `a`.
async fn two_packages() -> (Fixture, ChangesetId, ChangesetId) {
    let f = Fixture::new().await;
    let c0 = f
        .commit(
            &[],
            &[
                ("package.json", ROOT.as_bytes()),
                ("pkgs/a/package.json", A.as_bytes()),
                ("pkgs/a/src/index.js", b"1"),
                ("pkgs/b/package.json", B.as_bytes()),
                ("pkgs/b/src/index.js", b"1"),
                ("README.md", b"hi"),
            ],
        )
        .await;
    let c1 = f.commit(&[c0], &[("pkgs/a/src/index.js", b"2")]).await;
    f.set_branch("master", c0).await;
    set_ref(&f, "refs/remotes/origin/master", c0).await;
    f.set_branch("feat", c1).await;
    f.checkout_branch("feat", Some(c1)).await;
    (f, c0, c1)
}

fn asc(f: &Fixture, args: &[&str]) -> Output {
    f.asc().args(args).output().expect("spawn asc")
}

fn ok(f: &Fixture, args: &[&str]) -> String {
    let out = asc(f, args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn code(f: &Fixture, args: &[&str]) -> (i32, String) {
    let out = asc(f, args);
    (out.status.code().unwrap_or(-1), text(&out.stderr))
}

#[tokio::test]
async fn default_base_is_origin_default_branch_not_the_upstream() {
    let (f, _, tip) = two_packages().await;
    // The branch's upstream is origin/feat, which is at the branch tip.
    for (k, v) in [
        ("branch.feat.remote", "origin"),
        ("branch.feat.merge", "refs/heads/feat"),
    ] {
        ok(&f, &["config", k, v]);
    }
    set_ref(&f, "refs/remotes/origin/feat", tip).await;
    assert_eq!(ok(&f, &["affected"]), "node:a\n");
    // An explicit base wins.
    assert_eq!(
        ok(&f, &["affected", "--base", "refs/remotes/origin/feat"]),
        ""
    );
}

#[tokio::test]
async fn base_falls_back_to_the_local_default_branch() {
    // No origin refs at all: master is used.
    let g = Fixture::new().await;
    let c0 = g
        .commit(
            &[],
            &[("pkgs/a/package.json", A.as_bytes()), ("pkgs/a/x", b"1")],
        )
        .await;
    let c1 = g.commit(&[c0], &[("pkgs/a/x", b"2")]).await;
    g.set_branch("master", c0).await;
    g.set_branch("feat", c1).await;
    g.checkout_branch("feat", Some(c1)).await;
    assert_eq!(ok(&g, &["affected"]), "node:a\n");
}

#[tokio::test]
async fn formats() {
    let (f, c0, c1) = two_packages().await;
    assert_eq!(
        ok(&f, &["affected", "--explain"]),
        "node:a\tfile:pkgs/a/src/index.js\n"
    );
    assert_eq!(ok(&f, &["affected", "--format", "turbo"]), "--filter=a\n");
    assert_eq!(ok(&f, &["affected", "--format", "nx"]), "a\n");
    let matrix: Value = serde_json::from_str(&ok(&f, &["affected", "--format", "matrix"])).unwrap();
    assert_eq!(matrix["include"][0]["project"], "node:a");
    assert_eq!(matrix["include"][0]["root"], "pkgs/a");
    let gha = ok(&f, &["affected", "--format", "gha"]);
    for line in [
        "count=1",
        "all=false",
        "node=true",
        "rust=false",
        "turbo_filter=--filter=a",
        "nx_projects=a",
    ] {
        assert!(gha.lines().any(|l| l == line), "{line} in {gha}");
    }
    assert!(gha.starts_with("affected=[\"node:a\"]\n"));

    let doc: Value = serde_json::from_str(&ok(&f, &["affected", "--json", "--hashes"])).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["all"], false);
    assert_eq!(doc["projects"].as_array().unwrap().len(), 1);
    let p = &doc["projects"][0];
    assert_eq!(
        (
            p["id"].as_str(),
            p["ecosystem"].as_str(),
            p["kind"].as_str()
        ),
        (Some("node:a"), Some("node"), Some("package"))
    );
    assert_eq!(p["direct"], true);
    assert_eq!(p["hash"].as_str().map(str::len), Some(64));
    assert_eq!(doc["unowned"], Value::Array(vec![]));
    let (b, h) = (doc["base"].as_str().unwrap(), doc["head"].as_str().unwrap());
    assert_eq!(b.len(), 40);
    assert_eq!(doc["mergeBase"], doc["base"]);
    assert_ne!(b, h);
    assert_eq!(b, f.engine.git_commit_of(c0).await.unwrap().to_string());
    assert_eq!(h, f.engine.git_commit_of(c1).await.unwrap().to_string());
}

#[tokio::test]
async fn target_and_type_and_select_filter() {
    let (f, ..) = two_packages().await;
    assert_eq!(ok(&f, &["affected", "--type", "rust"]), "");
    assert_eq!(ok(&f, &["affected", "--type", "js"]), "node:a\n");
    assert_eq!(ok(&f, &["affected", "--select", "node:a"]), "node:a\n");
    assert_eq!(ok(&f, &["affected", "--select", "node:b"]), "");
    assert_eq!(
        ok(&f, &["affected", "--no-merge-base", "--base", "HEAD"]),
        ""
    );
}

#[tokio::test]
async fn exit_codes() {
    let (f, ..) = two_packages().await;
    assert_eq!(code(&f, &["affected", "--base", "nosuchrev"]).0, 128);
    assert_eq!(code(&f, &["affected", "--head", "nosuchrev"]).0, 128);
    let (c, err) = code(&f, &["affected", "--select", "node:nosuch"]);
    assert_eq!(c, 1, "{err}");
    assert_eq!(code(&f, &["affected", "--format", "xml"]).0, 129);
    assert_eq!(code(&f, &["affected", "--type", "cobol"]).0, 129);
    assert_eq!(code(&f, &["affected", "--bogus"]).0, 129);
    assert_eq!(code(&f, &["projects", "node:nosuch"]).0, 1);
    assert_eq!(code(&f, &["graph", "--rev", "nosuchrev"]).0, 128);
    assert_eq!(code(&f, &["affected", "--repo", "o/r"]).0, 128);
}

#[tokio::test]
async fn invalid_projects_toml_exits_1() {
    let f = Fixture::new().await;
    let c0 = f
        .commit(&[], &[("pkgs/a/package.json", A.as_bytes())])
        .await;
    let c1 = f
        .commit(&[c0], &[(".asc/projects.toml", b"[global\nnot toml")])
        .await;
    f.set_branch("master", c0).await;
    f.set_branch("feat", c1).await;
    f.checkout_branch("feat", Some(c1)).await;
    let (c, err) = code(&f, &["affected"]);
    assert_eq!(c, 1, "{err}");
    assert!(
        err.contains("asc:") || err.contains("projects.toml"),
        "{err}"
    );
}

#[tokio::test]
async fn projects_listing_and_owner() {
    let (f, ..) = two_packages().await;
    assert_eq!(ok(&f, &["projects"]), "node:root\nnode:a\nnode:b\n");
    assert_eq!(ok(&f, &["projects", "node:b"]), "node:b\n");
    assert_eq!(ok(&f, &["projects", "--type", "rust"]), "");
    let doc: Value = serde_json::from_str(&ok(&f, &["projects", "--format", "json"])).unwrap();
    assert_eq!(doc["projects"].as_array().unwrap().len(), 3);
    assert_eq!(doc["rev"].as_str().map(str::len), Some(40));
    let owners = ok(
        &f,
        &[
            "projects",
            "--owner",
            "pkgs/a/src/index.js",
            "--owner",
            "pkgs/a/package.json",
            "--owner",
            "nothing.txt",
        ],
    );
    let lines: Vec<&str> = owners.lines().collect();
    assert_eq!(lines[0], "pkgs/a/src/index.js\tnode:a");
    assert_eq!(lines[1], "pkgs/a/package.json\tnode:a");
    assert!(lines[2].starts_with("nothing.txt\t"), "{owners}");
    assert_eq!(code(&f, &["projects", "--owner", "../x"]).0, 129);
}

#[tokio::test]
async fn graph_json_and_dot() {
    let (f, ..) = two_packages().await;
    let doc: Value = serde_json::from_str(&ok(&f, &["graph"])).unwrap();
    let ids: Vec<&str> = doc["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["node:root", "node:a", "node:b"]);
    assert_eq!(doc["projects"][0]["hash"], Value::Null);
    let hashed: Value =
        serde_json::from_str(&ok(&f, &["graph", "--hashes", "--target", "build"])).unwrap();
    assert_eq!(
        hashed["projects"][0]["hash"].as_str().map(str::len),
        Some(64)
    );
    let dot = ok(&f, &["graph", "--format", "dot"]);
    assert!(dot.starts_with("digraph projects {\n"), "{dot}");
    assert!(
        dot.contains("  \"node:a\";\n") && dot.ends_with("}\n"),
        "{dot}"
    );
    assert_eq!(code(&f, &["graph", "--target", "build"]).0, 129);
}

#[tokio::test]
async fn uncommitted_adds_work_tree_paths() {
    let (f, ..) = two_packages().await;
    // Make the work tree match HEAD, then add an untracked file under `b`.
    for (path, body) in [
        ("package.json", ROOT),
        ("pkgs/a/package.json", A),
        ("pkgs/a/src/index.js", "2"),
        ("pkgs/b/package.json", B),
        ("pkgs/b/src/index.js", "1"),
        ("README.md", "hi"),
    ] {
        let p = f.root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    assert_eq!(ok(&f, &["affected"]), "node:a\n");
    fs::write(f.root.join("pkgs/b/src/new.js"), "x").unwrap();
    assert_eq!(ok(&f, &["affected", "--uncommitted"]), "node:a\nnode:b\n");
    // A dirty manifest affects its project.
    fs::remove_file(f.root.join("pkgs/b/src/new.js")).unwrap();
    fs::write(
        f.root.join("pkgs/b/package.json"),
        r#"{"name":"b","version":"2.0.0"}"#,
    )
    .unwrap();
    assert_eq!(
        ok(&f, &["affected", "--uncommitted", "--explain"])
            .lines()
            .count(),
        2
    );
}
