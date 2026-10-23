//! C31 acceptance: the Workspace plugin additions of SPEC 20.6 (`Stacks`, `Worktrees`,
//! `Conflicts`, `CommitGraph` with WIP rows, and `Blame` with an unsaved `buffer`) against the
//! matching `--json` CLI output on a fixture workspace. A real `asc daemon run` serves the RPCs on a
//! private endpoint (`ASC_DAEMON_ENDPOINT`), so the user's own daemon is never involved.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use asc_core::id::ChangesetId;
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use serde_json::Value;
use support::{Fixture, text};
use tonic::Code;
use tonic::transport::Channel;

const A0: &[u8] = b"l1\nl2\nl3\n";
const A_FEAT: &[u8] = b"l1\nl2 feat\nl3\n";
const A_OTHER: &[u8] = b"l1\nl2 other\nl3\n";

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
    c2: ChangesetId,
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

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-c31-{}-{}",
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `master` at c0, `feat` (checked out) at c1 one change ahead, `other` at c2 (conflicts with c1
/// on `a.txt`); the work tree holds c1; a daemon serves it.
async fn rig() -> Rig {
    let f = Fixture::new().await;
    let c0 = f.commit(&[], &[("a.txt", A0), ("b.txt", b"b\n")]).await;
    let c1 = f.commit(&[c0], &[("a.txt", A_FEAT)]).await;
    let c2 = f.commit(&[c0], &[("a.txt", A_OTHER)]).await;
    f.set_branch("master", c0).await;
    f.set_branch("feat", c1).await;
    f.set_branch("other", c2).await;
    f.checkout_branch("feat", Some(c1)).await;
    write(&f.root, "a.txt", A_FEAT);
    write(&f.root, "b.txt", b"b\n");

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
        c2,
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

fn first_json(out: &str) -> Value {
    serde_json::from_str(out.lines().find(|l| !l.trim().is_empty()).unwrap()).unwrap()
}

#[tokio::test]
async fn stacks_are_asc_sl_json() {
    let mut r = rig().await;
    // Index the fixture's commit on `feat` as change 1 (the sinks that mint ids belong to C04).
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
    let cli = first_json(&ok(&r.f, &r.ep, &["sl", "--json"]));
    let got = r
        .client
        .stacks(wire::WsStacksRequest {
            ws_id: r.ws.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.version, 1);
    assert_eq!(got.trunks.len(), cli["trunks"].as_array().unwrap().len());
    assert_eq!(got.trunks[0].name, "master");
    assert_eq!(
        hex(&got.trunks[0].tip),
        cli["trunks"][0]["tip"].as_str().unwrap()
    );
    assert_eq!(got.stacks.len(), 1, "{cli}");
    let (s, c) = (&got.stacks[0], &cli["stacks"][0]);
    assert_eq!(s.trunk, "master");
    assert_eq!(hex(&s.base), c["base"].as_str().unwrap());
    assert_eq!(s.changes.len(), c["changes"].as_array().unwrap().len());
    let e = &s.changes[0];
    let ce = &c["changes"][0];
    assert_eq!(hex(&e.commit), ce["commit"].as_str().unwrap());
    assert_eq!(hex(&e.asc), ce["asc"].as_str().unwrap());
    assert_eq!(e.short, ce["short"].as_str().unwrap());
    assert_eq!(e.branches, ["feat"]);
    assert!(e.current);
    assert_eq!(e.state, "draft");
    assert_eq!(
        e.author.as_ref().unwrap().email,
        b"test@example.com".to_vec()
    );
    assert_eq!(
        e.author.as_ref().unwrap().time,
        ce["author"]["time"].as_i64().unwrap()
    );
    assert!(e.review.is_none());
    assert_eq!(e.change.as_ref(), [1u8; 16]);

    // The graph row of the same commit carries the same change id.
    let page = collect(
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(wire::CommitGraphRequest {
                    include: vec!["feat".into()],
                    limit: 1,
                    ..Default::default()
                }),
                wip_rows: false,
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let row = page[0].rows[0].entry.as_ref().unwrap();
    assert_eq!(row.change, e.change);
    assert!(!row.change.is_empty());

    // A revset that selects nothing gives no stacks; a bad one is refused.
    let none = r
        .client
        .stacks(wire::WsStacksRequest {
            ws_id: r.ws.clone(),
            revset: "none()".into(),
            ..Default::default()
        })
        .await;
    if let Ok(none) = none {
        assert!(none.into_inner().stacks.is_empty());
    }
    let bad = r
        .client
        .stacks(wire::WsStacksRequest {
            ws_id: r.ws.clone(),
            revset: "((".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::FailedPrecondition);
    let nl = r
        .client
        .stacks(wire::WsStacksRequest {
            ws_id: r.ws.clone(),
            revset: "a\nb".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(nl.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn unknown_and_unmounted_workspaces_are_refused() {
    let mut r = rig().await;
    let e = r
        .client
        .stacks(wire::WsStacksRequest {
            ws_id: "nonsense".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let other = asc_state::WorkspaceId::new().to_string();
    for code in [
        r.client
            .conflicts(wire::WsConflictsRequest {
                ws_id: other.clone(),
            })
            .await
            .unwrap_err()
            .code(),
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: other.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .code(),
    ] {
        assert_eq!(code, Code::FailedPrecondition);
    }
}

#[tokio::test]
async fn worktrees_list_the_mounted_repository() {
    let mut r = rig().await;
    let got = r
        .client
        .worktrees(wire::WsWorktreesRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.worktrees.len(), 1);
    let w = &got.worktrees[0];
    assert_eq!(w.ws_id, r.ws);
    assert_eq!(w.mode, "plain");
    assert_eq!(w.head_ref, "refs/heads/feat");
    assert_eq!(w.head.len(), 20, "a git commit id");
    assert_eq!((w.staged, w.ahead, w.behind), (0, 0, 0));
    assert_eq!(w.locked, "");
    assert_eq!(w.in_progress, "");
    assert_eq!(w.ephemeral_until, 0);
    assert_eq!(w.path, r.f.root.display().to_string());

    // Staging a change shows in `staged`; an upstream shows in `ahead`.
    write(&r.f.root, "b.txt", b"changed\n");
    ok(&r.f, &r.ep, &["add", "b.txt"]);
    ok(&r.f, &r.ep, &["config", "branch.feat.remote", "."]);
    ok(
        &r.f,
        &r.ep,
        &["config", "branch.feat.merge", "refs/heads/master"],
    );
    let w = r
        .client
        .worktrees(wire::WsWorktreesRequest {})
        .await
        .unwrap()
        .into_inner()
        .worktrees
        .remove(0);
    assert_eq!(w.staged, 1);
    assert_eq!((w.ahead, w.behind), (1, 0));
}

#[tokio::test]
async fn conflicts_come_from_staging_stages() {
    let mut r = rig().await;
    let none = r
        .client
        .conflicts(wire::WsConflictsRequest {
            ws_id: r.ws.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(none.conflicts.is_empty());

    let out = asc(&r.f, &r.ep, &["merge", "other"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let got = r
        .client
        .conflicts(wire::WsConflictsRequest {
            ws_id: r.ws.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.conflicts.len(), 1);
    let c = &got.conflicts[0];
    assert_eq!(c.path.as_ref(), b"a.txt");
    assert_eq!(c.kind, wire::ConflictKind::Content as i32);
    for side in [&c.base, &c.ours, &c.theirs] {
        assert_eq!(side.len(), 20, "a git blob id");
    }
    assert_ne!(c.ours, c.theirs);
    let marked = String::from_utf8_lossy(&c.marked).into_owned();
    assert!(
        marked.contains("<<<<<<<") && marked.contains("l2 feat") && marked.contains("l2 other"),
        "{marked}"
    );

    // The WIP row of a worktree in the middle of a merge, and its worktree row.
    let w = r
        .client
        .worktrees(wire::WsWorktreesRequest {})
        .await
        .unwrap()
        .into_inner()
        .worktrees
        .remove(0);
    assert_eq!(w.in_progress, "merge");
}

#[tokio::test]
async fn commit_graph_pages_carry_lanes_refs_and_wip_rows() {
    let mut r = rig().await;
    let query = |limit: u32, token: &str| wire::CommitGraphRequest {
        preset: wire::GraphPreset::All as i32,
        lanes: true,
        limit,
        page_token: token.into(),
        ..Default::default()
    };
    let all = collect(
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(query(0, "")),
                wip_rows: false,
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let rows: Vec<&wire::GraphRow> = all.iter().flat_map(|m| m.rows.iter()).collect();
    assert_eq!(rows.len(), 3, "master, feat and other");
    assert!(rows.iter().all(|row| !row.wip));
    let has = |name: &str| {
        rows.iter()
            .any(|row| row.refs.iter().any(|x| x == &format!("refs/heads/{name}")))
    };
    assert!(has("master") && has("feat") && has("other"));
    // The root has no edges; the others point at it.
    let root = rows
        .iter()
        .find(|row| row.refs.iter().any(|x| x == "refs/heads/master"))
        .unwrap();
    assert!(root.edges.is_empty());
    for row in rows
        .iter()
        .filter(|row| row.entry.as_ref().unwrap().parents.len() == 1)
    {
        assert_eq!(row.edges.len(), 1);
        assert_eq!(row.edges[0].parent, row.entry.as_ref().unwrap().parents[0]);
    }
    let feat = rows
        .iter()
        .find(|row| row.refs.iter().any(|x| x == "refs/heads/feat"))
        .unwrap();
    let e = feat.entry.as_ref().unwrap();
    assert_eq!(e.id.as_ref(), r.c1.as_bytes());
    assert_eq!(e.git.len(), 20);

    // Two rows per page: the second page continues, and no row repeats.
    let p1 = collect(
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(query(2, "")),
                wip_rows: false,
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let n1: usize = p1.iter().map(|m| m.rows.len()).sum();
    let token = p1.last().unwrap().next_page_token.clone();
    assert_eq!(n1, 2);
    assert!(!token.is_empty());
    assert!(token.len() <= 8192 && token.bytes().all(|b| b.is_ascii_hexdigit()));
    let p2 = collect(
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(query(2, &token)),
                wip_rows: false,
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    assert_eq!(p2.iter().map(|m| m.rows.len()).sum::<usize>(), 1);
    assert!(p2.last().unwrap().next_page_token.is_empty());
    let mut ids: Vec<Vec<u8>> = p1
        .iter()
        .chain(&p2)
        .flat_map(|m| m.rows.iter())
        .map(|row| row.entry.as_ref().unwrap().id.to_vec())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);

    // Hostile input.
    for q in [
        wire::CommitGraphRequest {
            preset: 9,
            ..Default::default()
        },
        wire::CommitGraphRequest {
            page_token: "zz".into(),
            ..Default::default()
        },
        wire::CommitGraphRequest {
            include: vec!["a".repeat(2000)],
            ..Default::default()
        },
        wire::CommitGraphRequest {
            include: vec!["--all".into()],
            ..Default::default()
        },
        wire::CommitGraphRequest {
            include: vec!["x".into(); 65],
            ..Default::default()
        },
    ] {
        let e = r
            .client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(q),
                wip_rows: false,
            })
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);
    }
    let expired = r
        .client
        .commit_graph(wire::WsCommitGraphRequest {
            ws_id: r.ws.clone(),
            query: Some(query(2, "00")),
            wip_rows: false,
        })
        .await;
    let failed = match expired {
        Err(e) => Some(e),
        Ok(s) => s.into_inner().message().await.err(),
    };
    assert_eq!(
        failed.expect("a forged token fails").code(),
        Code::FailedPrecondition
    );

    // A staged change makes a WIP row on the first page only.
    write(&r.f.root, "b.txt", b"changed\n");
    ok(&r.f, &r.ep, &["add", "b.txt"]);
    let wip = collect(
        r.client
            .commit_graph(wire::WsCommitGraphRequest {
                ws_id: r.ws.clone(),
                query: Some(query(0, "")),
                wip_rows: true,
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let rows: Vec<&wire::GraphRow> = wip.iter().flat_map(|m| m.rows.iter()).collect();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].wip);
    assert_eq!(rows[0].ws_id, r.ws);
    assert_eq!(
        rows[0].entry.as_ref().unwrap().parents,
        [bytes::Bytes::copy_from_slice(r.c1.as_bytes())]
    );
    assert_eq!(rows.iter().filter(|row| row.wip).count(), 1);
    let _ = r.c2;
}

#[tokio::test]
async fn blame_maps_an_unsaved_buffer_onto_head() {
    let mut r = rig().await;
    let req = |buffer: &[u8], start: u32, end: u32, ws: bool, path: &str| wire::WsBlameRequest {
        ws_id: r.ws.clone(),
        path: path.as_bytes().to_vec().into(),
        buffer: buffer.to_vec().into(),
        start_line: start,
        end_line: end,
        ignore_whitespace: ws,
        ..Default::default()
    };
    let plain = collect(
        r.client
            .blame(wire::WsBlameRequest {
                ws_id: r.ws.clone(),
                path: b"a.txt".to_vec().into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let committed: Vec<(Vec<u8>, u32)> = plain
        .iter()
        .flat_map(|m| m.hunks.iter())
        .flat_map(|h| (0..h.len).map(move |k| (h.origin.to_vec(), h.start + k)))
        .collect();
    assert_eq!(committed.len(), 3);

    // A buffer equal to the file blames like the file.
    let same = collect(
        r.client
            .blame(req(A_FEAT, 0, 0, false, "a.txt"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let origins = |ms: &[wire::BlameResponse]| -> Vec<Vec<u8>> {
        ms.iter()
            .flat_map(|m| m.hunks.iter())
            .flat_map(|h| (0..h.len).map(move |_| h.origin.to_vec()))
            .collect()
    };
    assert_eq!(origins(&same), origins(&plain));
    assert!(same.iter().all(|m| m.commits.len() <= m.hunks.len()));

    // A line inserted at the top is uncommitted; the others keep their origins.
    let edited = collect(
        r.client
            .blame(req(b"NEW\nl1\nl2 feat\nl3\n", 0, 0, false, "a.txt"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let o = origins(&edited);
    assert_eq!(o.len(), 4);
    assert!(o[0].is_empty());
    assert_eq!(&o[1..], &origins(&plain)[..]);
    let first = &edited[0].hunks[0];
    assert_eq!((first.start, first.len), (1, 1));
    let second = &edited[0].hunks[1];
    assert_eq!((second.start, second.orig_start), (2, 1));

    // A range selects buffer lines.
    let ranged = collect(
        r.client
            .blame(req(b"NEW\nl1\nl2 feat\nl3\n", 3, 3, false, "a.txt"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let hunks: Vec<&wire::BlameHunk> = ranged.iter().flat_map(|m| m.hunks.iter()).collect();
    assert_eq!(hunks.len(), 1);
    assert_eq!(
        (hunks[0].start, hunks[0].len, hunks[0].orig_start),
        (3, 1, 2)
    );
    assert_eq!(hunks[0].origin.as_ref(), r.c1.as_bytes());
    assert!(
        ranged[0]
            .commits
            .iter()
            .any(|c| c.id.as_ref() == r.c1.as_bytes())
    );

    // Whitespace changes: uncommitted, unless whitespace is ignored.
    let strict = collect(
        r.client
            .blame(req(b"l1 \nl2 feat\nl3\n", 0, 0, false, "a.txt"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    assert!(origins(&strict)[0].is_empty());
    let lax = collect(
        r.client
            .blame(req(b"l1 \nl2 feat\nl3\n", 0, 0, true, "a.txt"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    assert!(!origins(&lax)[0].is_empty());

    // A file the revision lacks is wholly uncommitted.
    let fresh = collect(
        r.client
            .blame(req(b"x\ny\n", 0, 0, false, "new.txt"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    assert_eq!(origins(&fresh), vec![Vec::<u8>::new(), Vec::new()]);

    // Bad input.
    for bad in [
        req(b"x", 0, 2, false, "a.txt"),
        req(b"x", 0, 0, false, "../a.txt"),
        req(b"x", 0, 0, false, ""),
    ] {
        let e = match r.client.blame(bad).await {
            Err(e) => e,
            Ok(s) => s.into_inner().message().await.unwrap_err(),
        };
        assert_eq!(e.code(), Code::InvalidArgument);
    }
    // Ignoring whitespace without a buffer is `-w`.
    let w = collect(
        r.client
            .blame(wire::WsBlameRequest {
                ws_id: r.ws.clone(),
                path: b"a.txt".to_vec().into(),
                ignore_whitespace: true,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    assert_eq!(origins(&w), origins(&plain));
}
