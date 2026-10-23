//! P57 acceptance: the asc API additions of SPEC §24.8.2 against a real daemon on a private
//! endpoint (`ASC_DAEMON_ENDPOINT`). A2: the first `CommitGraph` page carries `total` and
//! `ref_markers[]` and later pages do not. A4: a `[[project]]` `argv` table is served as
//! `AffectedProject.targets` and a bad table is refused. A1: `WsStack.behind` counts the trunk
//! commits that are not in the stack base.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use asc_core::id::ChangesetId;
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use serde_json::Value;
use support::{Fixture, text};
use tonic::transport::Channel;

const A: &str = r#"{"name":"a","version":"1.0.0"}"#;
const B: &str = r#"{"name":"b","version":"1.0.0"}"#;
const ROOT: &str = r#"{"name":"root","private":true,"workspaces":["pkgs/*"]}"#;
const PROJECTS: &str = "[[project]]\nselect = \"node:a\"\nargv = { test = [\"bun\", \"run\", \"test\"], lint = [\"eslint\", \".\"] }\n";

const BASE_FILES: &[(&str, &[u8])] = &[
    ("package.json", ROOT.as_bytes()),
    ("pkgs/a/package.json", A.as_bytes()),
    ("pkgs/a/src/index.js", b"1\n"),
    ("pkgs/b/package.json", B.as_bytes()),
    ("pkgs/b/src/index.js", b"1\n"),
    (".asc/projects.toml", PROJECTS.as_bytes()),
];

struct Daemon {
    child: std::process::Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Rig {
    f: Fixture,
    ep: String,
    client: WorkspaceClient<Channel>,
    ws: String,
    c1: ChangesetId,
    _daemon: Daemon,
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn asc_command(f: &Fixture, ep: &str, args: &[&str]) -> Command {
    let mut c = f.asc();
    c.args(args)
        .env("ASC_NO_DAEMON", "1")
        .env("ASC_DAEMON_ENDPOINT", ep)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    c
}

fn asc(f: &Fixture, ep: &str, args: &[&str]) -> Output {
    asc_command(f, ep, args).output().expect("spawn asc")
}

fn ok(f: &Fixture, ep: &str, args: &[&str]) -> String {
    let out = asc(f, ep, args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn first_json(out: &str) -> Value {
    serde_json::from_str(out.lines().find(|l| !l.trim().is_empty()).unwrap()).unwrap()
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-p57-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    } else {
        f.sandbox
            .tmp
            .path()
            .join("ipc")
            .join("d.sock")
            .to_string_lossy()
            .into_owned()
    }
}

/// `master` at c2 (one trunk commit beyond c0); `feat` (checked out) at c1 on c0; a daemon
/// serves the workspace.
async fn rig() -> Rig {
    let f = Fixture::new().await;
    let c0 = f.commit(&[], BASE_FILES).await;
    let c1 = f.commit(&[c0], &[("pkgs/a/src/index.js", b"2\n")]).await;
    let c2 = f.commit(&[c0], &[("pkgs/b/src/index.js", b"2\n")]).await;
    f.set_branch("master", c2).await;
    f.set_branch("feat", c1).await;
    f.checkout_branch("feat", Some(c1)).await;
    for (path, content) in BASE_FILES {
        write(&f.root, path, content);
    }
    write(&f.root, "pkgs/a/src/index.js", b"2\n");

    let ep = endpoint_name(&f);
    let child = f
        .asc()
        .args(["daemon", "run"])
        .env("ASC_DAEMON_ENDPOINT", &ep)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let daemon = Daemon { child };
    let mut client = WorkspaceClient::new(connect(&IpcEndpoint::from_raw(ep.clone())));
    let mut up = false;
    for _ in 0..200 {
        if client.ping(wire::PingRequest {}).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the daemon did not come up");
    let ws = f.ws.to_string();
    client
        .mount(wire::MountRequest { ws_id: ws.clone() })
        .await
        .unwrap();
    Rig {
        f,
        ep,
        client,
        ws,
        c1,
        _daemon: daemon,
    }
}

async fn collect<T>(mut s: tonic::Streaming<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Some(m) = tokio::time::timeout(Duration::from_secs(60), s.message())
        .await
        .expect("stream stalled")
        .unwrap()
    {
        v.push(m);
    }
    v
}

async fn graph(r: &mut Rig, limit: u32, token: &str) -> Vec<wire::CommitGraphResponse> {
    collect(
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(wire::CommitGraphRequest {
                    include: vec!["master".into(), "feat".into()],
                    limit,
                    page_token: token.into(),
                    ..Default::default()
                }),
                wip_rows: false,
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await
}

#[tokio::test]
async fn the_first_graph_page_carries_total_and_ref_markers() {
    let mut r = rig().await;
    let got = graph(&mut r, 0, "").await;
    let rows: Vec<&wire::GraphRow> = got.iter().flat_map(|m| &m.rows).collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(got[0].total, 3, "the only page knows its total");
    for later in &got[1..] {
        assert_eq!((later.total, later.ref_markers.len()), (0, 0));
    }
    let mut names: Vec<(&str, u64)> = got[0]
        .ref_markers
        .iter()
        .map(|m| (m.name.as_str(), m.row))
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    for (name, row) in names {
        assert!(
            rows[row as usize].refs.iter().any(|x| x == name),
            "{name} labels row {row}"
        );
    }
}

#[tokio::test]
async fn later_pages_and_open_ended_first_pages_omit_the_total() {
    let mut r = rig().await;
    let first = graph(&mut r, 1, "").await;
    assert_eq!(
        first[0].total, 0,
        "more pages follow, so the total is not known"
    );
    assert_eq!(first[0].ref_markers.len(), 1);
    assert_eq!(first[0].ref_markers[0].row, 0);
    let token = first.last().unwrap().next_page_token.clone();
    assert!(!token.is_empty());
    let second = graph(&mut r, 1, &token).await;
    assert!(
        second
            .iter()
            .all(|m| m.total == 0 && m.ref_markers.is_empty())
    );
}

#[tokio::test]
async fn affected_serves_the_argv_table_as_targets() {
    let mut r = rig().await;
    let got = r
        .client
        .affected(wire::WsAffectedRequest {
            ws_id: r.ws.clone(),
            query: Some(wire::AffectedRequest {
                base: "master".into(),
                no_merge_base: true,
                ..Default::default()
            }),
            uncommitted: false,
        })
        .await
        .unwrap()
        .into_inner()
        .result
        .unwrap();
    let a = got
        .projects
        .iter()
        .find(|p| p.project.as_ref().unwrap().id == "node:a")
        .expect("project a is affected");
    assert_eq!(a.targets.len(), 2);
    assert_eq!(a.targets["test"].argv, ["bun", "run", "test"]);
    assert_eq!(a.targets["lint"].argv, ["eslint", "."]);
    // Projects with no table have no targets.
    assert!(
        got.projects
            .iter()
            .filter(|p| p.project.as_ref().unwrap().id != "node:a")
            .all(|p| p.targets.is_empty())
    );
    // The CLI JSON profile prints the same table.
    let cli = first_json(&ok(
        &r.f,
        &r.ep,
        &["affected", "--json", "--base", "master", "--no-merge-base"],
    ));
    let row = cli["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "node:a")
        .unwrap();
    assert_eq!(
        row["targets"]["test"]["argv"],
        serde_json::json!(["bun", "run", "test"])
    );
}

#[tokio::test]
async fn a_bad_argv_table_is_refused_with_its_line() {
    let r = rig().await;
    let bad =
        r.f.commit(
            &[r.c1],
            &[(
                ".asc/projects.toml",
                b"[[project]]
select = \"node:a\"
argv = { test = [] }
",
            )],
        )
        .await;
    r.f.set_branch("bad", bad).await;
    let out = asc(&r.f, &r.ep, &["affected", "--json", "--head", "bad"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("argv") && err.contains("3"), "{err}");
}

#[tokio::test]
async fn stack_behind_counts_trunk_commits_missing_from_the_base() {
    let mut r = rig().await;
    let rows = asc_state::changes::StateChanges::new(r.f.db.clone(), r.f.repo);
    asc_engine::change::ChangeRows::insert(
        &rows,
        &asc_engine::change::ChangeRow {
            cs: r.c1,
            change: asc_core::change::ChangeId::from_bytes([1; 16]),
            meta: asc_core::change::MetaId::from_bytes([1; 32]),
            op: asc_core::change::RewriteOp::New,
        },
        &[],
    )
    .await
    .unwrap();
    let got = r
        .client
        .stacks(wire::WsStacksRequest {
            ws_id: r.ws.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let cli = first_json(&ok(&r.f, &r.ep, &["sl", "--json"]));
    assert_eq!(got.stacks.len(), 1, "{cli}");
    // `feat` is based on c0; trunk `master` holds one commit (c2) that the base lacks.
    assert_eq!(got.stacks[0].behind, 1);
    assert_eq!(cli["stacks"][0]["behind"], 1);
}
