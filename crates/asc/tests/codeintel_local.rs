//! I19 acceptance: the local degraded index (SPEC §23.2.5). The helper `asc codeintel worker` runs
//! as a real process against a real workspace; the answers come from a real daemon on a private
//! endpoint with no asc server configured, so `$ASC_HOME/ci` is the only index. `coverage` says how
//! much of a revision the local index holds.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use asc_core::id::ChangesetId;
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use support::Fixture;
use tonic::transport::Channel;

const FILES: &[(&str, &[u8])] = &[
    (
        "src/lib.rs",
        b"pub fn parse_widget() -> u32 {\n    1\n}\n\npub struct Gadget;\n",
    ),
    (
        "src/user.rs",
        b"use crate::lib;\nfn run() -> u32 {\n    parse_widget() + 1\n}\n",
    ),
    (
        "docs/notes.txt",
        b"the parse_widget helper is documented here\n",
    ),
    ("data/blob.bin", &[0, 1, 2, 3, 0, 0, 255, 0, 9, 9]),
    (".env", b"API_TOKEN=abc123\n"),
];

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A one-commit workspace on `master`.
async fn workspace(files: &[(&str, &[u8])]) -> (Fixture, ChangesetId) {
    let f = Fixture::new().await;
    let c0 = f.commit(&[], files).await;
    f.set_branch("master", c0).await;
    f.checkout_branch("master", Some(c0)).await;
    for (p, c) in files {
        write(&f.root, p, c);
    }
    (f, c0)
}

fn ci_dir(f: &Fixture) -> PathBuf {
    f.home.root().join("ci")
}

fn repo_dir(f: &Fixture) -> PathBuf {
    std::fs::read_dir(ci_dir(f))
        .expect("ci dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("a repository directory")
}

fn revs(f: &Fixture) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(ci_dir(f)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for repo in rd.flatten().filter(|e| e.path().is_dir()) {
        if let Ok(rev) = std::fs::read_dir(repo.path().join("rev")) {
            for e in rev.flatten() {
                out.push(
                    e.file_name()
                        .to_string_lossy()
                        .trim_end_matches(".json")
                        .to_owned(),
                );
            }
        }
    }
    out.sort();
    out
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata().unwrap();
            total += if m.is_dir() {
                dir_size(&e.path())
            } else {
                m.len()
            };
        }
    }
    total
}

struct Run {
    ok: bool,
    out: String,
    err: String,
}

fn worker(f: &Fixture, args: &[&str], global: &[&str]) -> Run {
    let o = f
        .asc()
        .args(global)
        .args(["codeintel", "worker"])
        .args(args)
        .env("ASC_CODEINTEL_EXIT_SECS", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run the helper");
    Run {
        ok: o.status.success(),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn field(line: &str, name: &str) -> String {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {line:?}"))
        .to_owned()
}

// ----------------------------------------------------------------------------- the daemon

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Rig {
    client: WorkspaceClient<Channel>,
    ws: String,
    _daemon: Daemon,
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-i19-{}-{}",
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

async fn daemon(f: &Fixture) -> Rig {
    let ep = endpoint_name(f);
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
    let mut client = WorkspaceClient::new(connect(&IpcEndpoint::from_raw(ep)));
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
        client,
        ws,
        _daemon: daemon,
    }
}

fn rev_scope(cs: ChangesetId) -> Option<wire::Scope> {
    scope(wire::scope::At::Rev(cs.to_string()))
}

fn scope(at: wire::scope::At) -> Option<wire::Scope> {
    Some(wire::Scope {
        repo: Some(wire::RepoRef {
            name: "fixture".into(),
            id_mode: 0,
        }),
        at: Some(at),
        ..wire::Scope::default()
    })
}

impl Rig {
    async fn search(
        &mut self,
        scope: Option<wire::Scope>,
        pattern: &str,
    ) -> Vec<wire::SearchResponse> {
        let mut s = self
            .client
            .search(wire::WsSearchRequest {
                ws_id: self.ws.clone(),
                query: Some(wire::SearchRequest {
                    scope,
                    pattern: pattern.into(),
                    ..wire::SearchRequest::default()
                }),
            })
            .await
            .unwrap_or_else(|e| panic!("search {pattern:?}: {e}"))
            .into_inner();
        let mut out = Vec::new();
        while let Some(m) = tokio::time::timeout(Duration::from_secs(60), s.message())
            .await
            .expect("stream stalled")
            .unwrap()
        {
            out.push(m);
        }
        out
    }

    async fn symbols(
        &mut self,
        scope: Option<wire::Scope>,
        op: wire::SymbolOp,
        name: &str,
    ) -> wire::SymbolsResponse {
        self.client
            .symbols(wire::WsSymbolsRequest {
                ws_id: self.ws.clone(),
                query: Some(wire::SymbolsRequest {
                    scope,
                    op: op as i32,
                    name: name.into(),
                    ..wire::SymbolsRequest::default()
                }),
            })
            .await
            .unwrap()
            .into_inner()
    }

    async fn status(&mut self, scope: Option<wire::Scope>) -> wire::IndexStatusResponse {
        self.client
            .index_status(wire::WsIndexStatusRequest {
                ws_id: self.ws.clone(),
                query: Some(wire::IndexStatusRequest { scope }),
            })
            .await
            .unwrap()
            .into_inner()
    }
}

fn paths(msgs: &[wire::SearchResponse]) -> Vec<String> {
    let mut v: Vec<String> = msgs
        .iter()
        .flat_map(|m| &m.files)
        .map(|f| String::from_utf8_lossy(&f.path).into_owned())
        .collect();
    v.sort();
    v
}

// -------------------------------------------------------------------------------- tests

#[tokio::test]
async fn the_helper_indexes_head_and_reports_coverage() {
    let (f, c0) = workspace(FILES).await;
    let r = worker(&f, &["--once", "--now"], &[]);
    assert!(r.ok, "{}", r.err);
    let line = r
        .out
        .lines()
        .find(|l| l.starts_with("indexed "))
        .expect(&r.out);
    assert!(line.contains(&c0.to_string()), "{line}");
    assert_eq!(field(line, "files"), "5");
    assert_eq!(field(line, "missing"), "0");
    assert_eq!(field(line, "coverage"), "1.000");
    assert_eq!(
        field(line, "skipped"),
        "1",
        "the binary file is kept out: {line}"
    );
    assert_eq!(revs(&f), [c0.to_string()]);
    assert!(
        dir_size(&ci_dir(&f)) < asc_default_cap(),
        "the index stays inside codeintel.local.maxSize"
    );
    // Nothing is left behind: no lock, no scratch files.
    let repo = repo_dir(&f);
    assert!(!repo.join("lock").exists());
    assert_eq!(std::fs::read_dir(repo.join("work")).unwrap().count(), 0);
    // A second run finds everything in the table and analyses nothing.
    let again = worker(&f, &["--once", "--now"], &[]);
    assert!(again.ok, "{}", again.err);
    assert_eq!(field(again.out.lines().next().unwrap(), "segments"), "0");
}

fn asc_default_cap() -> u64 {
    512 << 20
}

#[tokio::test]
async fn the_daemon_answers_offline_from_the_local_index() {
    let (f, c0) = workspace(FILES).await;
    let mut rig = daemon(&f).await;

    // Before the helper ran there is nothing: an honest empty answer, coverage 0.
    let out = rig.search(rev_scope(c0), "parse_widget").await;
    assert!(paths(&out).is_empty());
    assert_eq!(out[0].scope.as_ref().unwrap().coverage, 0.0);

    let r = worker(&f, &["--once", "--now"], &[]);
    assert!(r.ok, "{}", r.err);

    let out = rig.search(rev_scope(c0), "parse_widget").await;
    assert_eq!(
        paths(&out),
        ["docs/notes.txt", "src/lib.rs", "src/user.rs"],
        "exact search verified against the files' bytes"
    );
    let resolved = out[0].scope.as_ref().unwrap();
    assert_eq!(resolved.coverage, 1.0);
    assert_eq!(resolved.changeset.as_ref(), c0.as_bytes());
    let lib = out[0]
        .files
        .iter()
        .find(|h| h.path.as_ref() == b"src/lib.rs")
        .unwrap();
    assert_eq!(lib.matches[0].line, 1);
    assert_eq!(lib.lang, "rust");

    // The secret file is searchable but redacted; the binary file is not indexed at all.
    let out = rig.search(rev_scope(c0), "API_TOKEN").await;
    let hit = &out[0].files[0];
    assert_eq!(hit.path.as_ref(), b".env");
    assert!(
        hit.redacted
            || hit
                .matches
                .iter()
                .all(|m| !String::from_utf8_lossy(&m.text).contains("abc123"))
    );
    assert!(paths(&rig.search(rev_scope(c0), "\\x00\\x01").await).is_empty());

    // Symbols come from the grammar tags in the index.
    let defs = rig
        .symbols(rev_scope(c0), wire::SymbolOp::Def, "parse_widget")
        .await;
    assert_eq!(defs.symbols.len(), 1, "{defs:?}");
    assert_eq!(defs.symbols[0].path.as_ref(), b"src/lib.rs");
    assert!(defs.symbols[0].is_def);
    let refs = rig
        .symbols(rev_scope(c0), wire::SymbolOp::Refs, "parse_widget")
        .await;
    assert!(
        refs.symbols
            .iter()
            .any(|s| s.path.as_ref() == b"src/user.rs" && !s.is_def)
    );

    // The index says what it is.
    let st = rig.status(rev_scope(c0)).await;
    assert_eq!(st.indexes.len(), 1);
    assert_eq!(st.indexes[0].index, "local");
    assert_eq!(st.indexes[0].coverage, 1.0);
    assert_eq!(st.scope.as_ref().unwrap().coverage, 1.0);

    // No model is loaded: semantic search has no vectors and says so.
    let sem = rig
        .client
        .semantic(wire::WsSemanticRequest {
            ws_id: rig.ws.clone(),
            query: Some(wire::SemanticRequest {
                scope: rev_scope(c0),
                query: "widget parser".into(),
                ..wire::SemanticRequest::default()
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(sem.chunks.is_empty());
    assert_eq!(sem.scope.unwrap().coverage, 0.0);

    // Workspace scope: the dirty file comes from the overlay, the clean ones from the index.
    write(
        &f.root,
        "src/user.rs",
        b"fn run() -> u32 {\n    parse_widget() + 2\n}\n",
    );
    let out = rig
        .search(scope(wire::scope::At::Workspace(true)), "parse_widget")
        .await;
    assert_eq!(
        paths(&out),
        ["docs/notes.txt", "src/lib.rs", "src/user.rs"],
        "no file twice: the stale indexed copy of the dirty file is dropped"
    );
    let user = out
        .iter()
        .flat_map(|m| &m.files)
        .find(|h| h.path.as_ref() == b"src/user.rs")
        .unwrap();
    assert!(String::from_utf8_lossy(&user.matches[0].text).contains("+ 2"));
    assert_eq!(out.last().unwrap().scope.as_ref().unwrap().coverage, 1.0);
}

#[tokio::test]
async fn a_file_missing_from_store_lowers_coverage_and_is_never_fetched() {
    let (f, c0) = workspace(FILES).await;
    let notes = f.file_leaf(c0, "docs/notes.txt").await.file;
    f.objects.delete(&ObjKey::File(notes)).await.unwrap();
    let r = worker(&f, &["--once", "--now"], &[]);
    assert!(r.ok, "{}", r.err);
    let line = r.out.lines().next().unwrap();
    assert_eq!(field(line, "missing"), "1", "{line}");
    let cov: f64 = field(line, "coverage").parse().unwrap();
    assert!((cov - 0.8).abs() < 1e-3, "4 of 5 files covered: {line}");
    assert!(
        !f.objects.has_many(&[ObjKey::File(notes)]).await.unwrap()[0],
        "the helper does not fetch or write the missing file"
    );

    let mut rig = daemon(&f).await;
    let out = rig.search(rev_scope(c0), "parse_widget").await;
    assert_eq!(paths(&out), ["src/lib.rs", "src/user.rs"]);
    let c = out[0].scope.as_ref().unwrap().coverage;
    assert!((c - 0.8).abs() < 1e-3, "{c}");
}

#[tokio::test]
async fn the_size_cap_evicts_the_least_recently_used_revision() {
    let (f, c1) = workspace(FILES).await;
    let r = worker(&f, &["--once", "--now"], &[]);
    assert!(r.ok, "{}", r.err);
    let first = dir_size(&ci_dir(&f));

    // A second revision with new content.
    let c2 = f
        .commit(&[c1], &[("src/extra.rs", b"pub fn extra_symbol() {}\n")])
        .await;
    f.set_branch("master", c2).await;
    f.checkout_branch("master", Some(c2)).await;
    write(&f.root, "src/extra.rs", b"pub fn extra_symbol() {}\n");

    // Room for one revision and a little more, not for two.
    let cap = (first + first / 4).to_string();
    let key = format!("codeintel.local.maxSize={cap}");
    let r = worker(&f, &["--once", "--now"], &["-c", &key]);
    assert!(r.ok, "{}", r.err);
    assert_eq!(revs(&f), [c2.to_string()], "the older revision was evicted");
    assert!(dir_size(&ci_dir(&f)) <= cap.parse().unwrap());

    // A cap that fits nothing fails loudly and leaves no revision behind.
    let (g, _) = workspace(FILES).await;
    let r = worker(
        &g,
        &["--once", "--now"],
        &["-c", "codeintel.local.maxSize=2k"],
    );
    assert!(!r.ok, "{}", r.out);
    assert!(r.err.contains("maxSize"), "{}", r.err);
    assert!(revs(&g).is_empty());
    assert!(dir_size(&ci_dir(&g)) <= 2048);
}

#[tokio::test]
async fn a_bad_size_is_refused() {
    let (f, _) = workspace(FILES).await;
    let r = worker(
        &f,
        &["--once", "--now"],
        &["-c", "codeintel.local.maxSize=lots"],
    );
    assert!(!r.ok);
    assert!(r.err.contains("codeintel.local.maxSize"), "{}", r.err);
}

#[tokio::test]
async fn the_helper_waits_for_an_idle_workspace_and_exits_when_it_has_no_job() {
    let (f, c0) = workspace(FILES).await;
    // The workspace was just written to: busy. The helper gives up at its exit time.
    let proj = asc_git::projection::Projection::open(&f.root).unwrap();
    proj.write_head(&asc_git::projection::Head::Branch("master".into()))
        .unwrap();
    let started = Instant::now();
    let r = worker(&f, &[], &[]);
    assert!(r.ok, "{}", r.err);
    assert!(revs(&f).is_empty(), "indexed a busy workspace: {}", r.out);
    assert!(started.elapsed() < Duration::from_secs(30));

    // Without the idle wait it indexes, then lingers for its exit time and goes.
    let started = Instant::now();
    let r = worker(&f, &["--now"], &[]);
    assert!(r.ok, "{}", r.err);
    assert_eq!(revs(&f), [c0.to_string()]);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the helper must exit by itself"
    );
    assert!(!repo_dir(&f).join("lock").exists());
}

#[tokio::test]
async fn grep_of_a_revision_is_identical_with_and_without_the_index() {
    let (f, c0) = workspace(FILES).await;
    let rev = c0.to_string();
    let run = |f: &Fixture| {
        let o = f
            .asc()
            .args(["grep", "-n", "-F", "parse_widget", &rev])
            .output()
            .unwrap();
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        )
    };
    let before = run(&f);
    assert_eq!(before.0, Some(0), "{before:?}");
    assert!(worker(&f, &["--once", "--now"], &[]).ok);
    let after = run(&f);
    assert_eq!(before, after, "the index only narrows the files read");
    let none = f
        .asc()
        .args(["grep", "-n", "-F", "no_such_symbol_anywhere", &rev])
        .output()
        .unwrap();
    assert_eq!(none.status.code(), Some(1));
}
