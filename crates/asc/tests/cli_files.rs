//! `asc files` (SPEC §5, `pathsearch.v1`): "go to file" over a revision's tree, driven through
//! the built binary on a repository the fixture builds directly.

mod support;

use support::{Fixture, text};

#[tokio::test]
async fn files_ranks_paths_and_follows_new_commits() {
    let f = Fixture::new().await;
    let a = f
        .commit(
            &[],
            &[
                ("src/main.rs", b"1"),
                ("src/mod/admin.rs", b"1"),
                ("docs/MAINTAINERS", b"1"),
                ("README.md", b"1"),
            ],
        )
        .await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;

    let out = f.asc().args(["files", "main"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let lines: Vec<String> = text(&out.stdout).lines().map(str::to_owned).collect();
    assert_eq!(lines[0], "src/main.rs");
    assert!(lines.contains(&"docs/MAINTAINERS".to_owned()));
    assert!(!lines.contains(&"README.md".to_owned()));

    // No query lists in order, `-n` caps it, `-z` ends with NUL.
    let out = f.asc().args(["files", "-n", "2", "-z"]).output().unwrap();
    assert_eq!(text(&out.stdout), "README.md\0docs/MAINTAINERS\0");

    // JSON lines carry the mode and the score.
    let out = f.asc().args(["files", "--json", "admin"]).output().unwrap();
    let v: serde_json::Value =
        serde_json::from_str(text(&out.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(v["path"], "src/mod/admin.rs");
    assert_eq!(v["mode"], "100644");
    assert!(v["score"].is_i64());

    // A later commit is searched through --rev; the earlier one still answers as it was.
    let b = f
        .commit(&[a], &[("src/main.rs", b"1"), ("src/maintenance.rs", b"2")])
        .await;
    f.set_branch("next", b).await;
    let out = f
        .asc()
        .args(["files", "--rev", "next", "maint"])
        .output()
        .unwrap();
    assert!(text(&out.stdout).lines().any(|l| l == "src/maintenance.rs"));
    let out = f.asc().args(["files", "maintenance"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "no match exits 1");

    // A byte outside ASCII is quoted as git quotes it (`core.quotePath`), and so would a control
    // byte be (Windows refuses one in a name); -z prints the name raw.
    let c = f.commit(&[b], &[("caf\u{e9}name", b"1")]).await;
    f.set_branch("accent", c).await;
    let out = f
        .asc()
        .args(["files", "--rev", "accent", "cafname"])
        .output()
        .unwrap();
    assert_eq!(text(&out.stdout), "\"caf\\303\\251name\"\n");
    let out = f
        .asc()
        .args(["files", "--rev", "accent", "-z", "cafname"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, "caf\u{e9}name\0".as_bytes());

    // Usage errors.
    let out = f.asc().args(["files", "a", "b"]).output().unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("usage: asc files"));
}
