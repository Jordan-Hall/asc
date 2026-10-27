//! I18 acceptance: the four code intelligence RPCs of the daemon over the IPC with workspace scope
//! (SPEC §23.2.5). The overlay answers for the dirty, staged and untracked files of a real
//! workspace served by a real `asc daemon run` on a private endpoint (`ASC_DAEMON_ENDPOINT`). No asc
//! server is configured, so the answers are the overlay's alone and `coverage` says so. The idle
//! test reads the daemon's own resident set from its loopback metrics endpoint.

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use support::Fixture;
use tonic::Code;
use tonic::transport::Channel;

const BASE_FILES: &[(&str, &[u8])] = &[
    ("README.md", b"hello\n"),
    ("src/lib.js", b"function stable() { return 1; }\n"),
    ("src/other.js", b"const x = 1;\n"),
];

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
    f: Fixture,
    client: WorkspaceClient<Channel>,
    ws: String,
    metrics: SocketAddr,
    pid: u32,
    _daemon: Daemon,
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-i18-{}-{}",
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

fn free_port() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// A clean one-commit workspace on `master` with a daemon that serves it. `idle_secs` is the
/// daemon's overlay idle time.
async fn rig(idle_secs: u64) -> Rig {
    let f = Fixture::new().await;
    let c0 = f.commit(&[], BASE_FILES).await;
    f.set_branch("master", c0).await;
    f.checkout_branch("master", Some(c0)).await;
    for (path, content) in BASE_FILES {
        write(&f.root, path, content);
    }
    let ep = endpoint_name(&f);
    let metrics = free_port();
    let child = f
        .asc()
        .args(["daemon", "run", "--metrics-listen", &metrics.to_string()])
        .env("ASC_DAEMON_ENDPOINT", &ep)
        .env("ASC_OVERLAY_IDLE_SECS", idle_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let pid = child.id();
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
        f,
        client,
        ws,
        metrics,
        pid,
        _daemon: daemon,
    }
}

fn scope(at: wire::scope::At) -> Option<wire::Scope> {
    Some(wire::Scope {
        repo: Some(wire::RepoRef {
            name: "acme/app".into(),
            id_mode: 0,
        }),
        at: Some(at),
        ..wire::Scope::default()
    })
}

fn workspace() -> Option<wire::Scope> {
    scope(wire::scope::At::Workspace(true))
}

impl Rig {
    async fn search(&mut self, pattern: &str) -> Vec<wire::SearchResponse> {
        self.search_with(wire::SearchRequest {
            scope: workspace(),
            pattern: pattern.into(),
            ..wire::SearchRequest::default()
        })
        .await
        .unwrap()
    }

    async fn search_with(
        &mut self,
        query: wire::SearchRequest,
    ) -> Result<Vec<wire::SearchResponse>, tonic::Status> {
        let mut s = self
            .client
            .search(wire::WsSearchRequest {
                ws_id: self.ws.clone(),
                query: Some(query),
            })
            .await?
            .into_inner();
        let mut out = Vec::new();
        while let Some(m) = tokio::time::timeout(Duration::from_secs(60), s.message())
            .await
            .expect("stream stalled")?
        {
            out.push(m);
        }
        Ok(out)
    }

    async fn symbols(
        &mut self,
        op: wire::SymbolOp,
        name: &str,
        path: &str,
    ) -> wire::SymbolsResponse {
        self.client
            .symbols(wire::WsSymbolsRequest {
                ws_id: self.ws.clone(),
                query: Some(wire::SymbolsRequest {
                    scope: workspace(),
                    op: op as i32,
                    name: name.into(),
                    path: path.as_bytes().to_vec().into(),
                    ..wire::SymbolsRequest::default()
                }),
            })
            .await
            .unwrap()
            .into_inner()
    }

    async fn status(&mut self) -> wire::IndexStatusResponse {
        self.client
            .index_status(wire::WsIndexStatusRequest {
                ws_id: self.ws.clone(),
                query: Some(wire::IndexStatusRequest { scope: workspace() }),
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

/// The daemon's `asc_rss_bytes` from its loopback metrics endpoint.
fn rss(addr: SocketAddr) -> u64 {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("metrics");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    body.lines()
        .find_map(|l| l.strip_prefix("asc_rss_bytes "))
        .unwrap_or_else(|| panic!("no asc_rss_bytes in {body}"))
        .trim()
        .parse()
        .unwrap()
}

/// The memory the daemon owns: anonymous plus shared memory from `/proc/<pid>/status`, which
/// leaves out the file-backed pages of the executable (shared, evictable and sized by the debug
/// binary, which links the whole server). Where the kernel gives no split, the daemon's metric.
fn owned_memory(pid: u32, metrics: SocketAddr) -> u64 {
    let field = |text: &str, key: &str| -> Option<u64> {
        let line = text.lines().find_map(|l| l.strip_prefix(key))?;
        line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
    };
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|t| Some((field(&t, "RssAnon:")? + field(&t, "RssShmem:")?) << 10))
        .unwrap_or_else(|| rss(metrics))
}

#[tokio::test]
async fn workspace_search_answers_dirty_staged_and_untracked_files_only() {
    let mut r = rig(300).await;
    // A clean workspace has an empty overlay.
    assert!(paths(&r.search("stable").await).is_empty());

    write(
        &r.f.root,
        "src/lib.js",
        b"function stable() { return 2; }\nfunction fresh() {}\n",
    );
    write(&r.f.root, "src/new.js", b"function fresh() { return 3; }\n");
    write(&r.f.root, "notes.txt", b"fresh notes\n");
    let out = r.search("fresh").await;
    assert_eq!(paths(&out), ["notes.txt", "src/lib.js", "src/new.js"]);
    let lib = out
        .iter()
        .flat_map(|m| &m.files)
        .find(|f| f.path.as_ref() == b"src/lib.js")
        .unwrap();
    assert_eq!(lib.matches[0].line, 2);
    assert_eq!(lib.lang, "javascript");
    assert!(!lib.redacted);
    // The clean file is the server's to answer, and there is no server.
    assert!(!paths(&out).contains(&"src/other.js".to_string()));
    let scope = out[0].scope.as_ref().unwrap();
    assert_eq!(scope.coverage, 0.0, "the base revision was not searched");
    assert!(!scope.changeset.is_empty(), "the answer cites HEAD");

    // A later edit is seen by the next query.
    write(&r.f.root, "notes.txt", b"nothing\n");
    assert_eq!(
        paths(&r.search("fresh").await),
        ["src/lib.js", "src/new.js"]
    );
    // Path filters and modes.
    let one = r
        .search_with(wire::SearchRequest {
            scope: Some(wire::Scope {
                paths: vec!["src/new.js".into()],
                ..workspace().unwrap()
            }),
            pattern: "FRESH".into(),
            case_sensitive: false,
            mode: wire::SearchMode::Word as i32,
            ..wire::SearchRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(paths(&one), ["src/new.js"]);
}

#[tokio::test]
async fn a_secret_file_in_the_overlay_is_redacted() {
    let mut r = rig(300).await;
    write(
        &r.f.root,
        ".env",
        b"API_KEY=abcdefghijklmnopqrstuvwxyz0123456789\n",
    );
    let out = r.search("API_KEY").await;
    let f = out.iter().flat_map(|m| &m.files).next().unwrap();
    assert!(f.redacted);
    assert!(f.matches.iter().all(|m| m.text.is_empty()), "{f:?}");
}

#[tokio::test]
async fn a_symlinked_directory_is_not_followed() {
    let mut r = rig(300).await;
    let outside = r.f.sandbox.tmp.path().join("outside");
    write(&outside, "leak.txt", b"leaked needle\n");
    let link = r.f.root.join("lnk");
    let made = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success())
    } else {
        #[cfg(unix)]
        let ok = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(not(unix))]
        let ok = false;
        ok
    };
    if !made {
        eprintln!("skipped: no link could be made here");
        return;
    }
    assert!(
        paths(&r.search("needle").await).is_empty(),
        "a file below a link is outside the work tree"
    );
}

#[tokio::test]
async fn symbols_and_index_status_over_the_overlay() {
    let mut r = rig(300).await;
    write(
        &r.f.root,
        "src/new.js",
        b"function target() { return 1; }\nfunction user() { return target(); }\n",
    );
    let defs = r.symbols(wire::SymbolOp::Def, "target", "").await;
    assert_eq!(defs.symbols.len(), 1, "{defs:?}");
    assert_eq!(defs.symbols[0].path.as_ref(), b"src/new.js");
    assert!(defs.symbols[0].is_def);
    assert_eq!(defs.symbols[0].line, 1);
    let callers = r.symbols(wire::SymbolOp::Callers, "target", "").await;
    assert_eq!(callers.symbols.len(), 1, "{callers:?}");
    assert_eq!(callers.symbols[0].name, "user");
    let outline = r.symbols(wire::SymbolOp::Outline, "", "src/new.js").await;
    assert_eq!(outline.symbols.len(), 2);

    let st = r.status().await;
    let overlay = st.indexes.iter().find(|i| i.index == "overlay").unwrap();
    assert!((overlay.coverage - 1.0).abs() < f32::EPSILON);
    assert!(overlay.generation >= 1);

    // Semantic search has only a lexical overlay answer, and it never needs a model.
    let sem = r
        .client
        .semantic(wire::WsSemanticRequest {
            ws_id: r.ws.clone(),
            query: Some(wire::SemanticRequest {
                scope: workspace(),
                query: "target return".into(),
                ..wire::SemanticRequest::default()
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(sem.chunks.iter().any(|c| c.path.as_ref() == b"src/new.js"));
    assert!(sem.chunks.iter().all(|c| c.sources == ["overlay"]));
}

#[tokio::test]
async fn refusals() {
    let mut r = rig(300).await;
    // Another scope has no local answer without a server.
    let e = r
        .search_with(wire::SearchRequest {
            scope: scope(wire::scope::At::Rev("master".into())),
            pattern: "x".into(),
            ..wire::SearchRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition, "{e}");
    // A bad regex, an empty pattern and a missing scope are the caller's mistakes.
    let e = r
        .search_with(wire::SearchRequest {
            scope: workspace(),
            pattern: "(unclosed".into(),
            mode: wire::SearchMode::Regex as i32,
            ..wire::SearchRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument, "{e}");
    let e = r
        .search_with(wire::SearchRequest {
            scope: workspace(),
            ..wire::SearchRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument, "{e}");
    let e = r
        .search_with(wire::SearchRequest {
            pattern: "x".into(),
            ..wire::SearchRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument, "{e}");
    // An unmounted or malformed workspace id is refused before any file is read.
    for ws in ["not-an-id", "00000000-0000-0000-0000-000000000000"] {
        let e = r
            .client
            .search(wire::WsSearchRequest {
                ws_id: ws.into(),
                query: Some(wire::SearchRequest {
                    scope: workspace(),
                    pattern: "x".into(),
                    ..wire::SearchRequest::default()
                }),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                e.code(),
                Code::InvalidArgument | Code::FailedPrecondition | Code::NotFound
            ),
            "{e}"
        );
    }
}

#[tokio::test]
async fn idle_rss_is_low_after_the_reader_closes() {
    let mut r = rig(1).await;
    // The daemon's resident set before any reader opens. A debug daemon is mostly code pages, so
    // the budget is the SPEC's idle 64 MiB (§16.4) or the baseline plus what the overlay may
    // hold (its 16 MiB heap bound and the code pages a query touches), whichever is larger: the
    // test measures what the reader leaves behind, not the binary it runs in.
    let baseline = owned_memory(r.pid, r.metrics);
    let limit = (64u64 << 20).max(baseline + (24 << 20));
    // 24 dirty files of about 900 KiB each: more than the overlay's 16 MiB heap bound.
    let body = "word another line of text\n".repeat(36_000);
    for i in 0..24 {
        write(&r.f.root, &format!("big/f{i}.txt"), body.as_bytes());
    }
    let out = r.search("another line").await;
    assert!(!paths(&out).is_empty());
    let cov = out[0].scope.as_ref().unwrap().coverage;
    assert_eq!(cov, 0.0);
    let st = r.status().await;
    let overlay = st.indexes.iter().find(|i| i.index == "overlay").unwrap();
    assert!(
        overlay.coverage < 1.0,
        "the heap bound left files out: {}",
        overlay.coverage
    );
    // The reader is gone: no stream or client call is open. The overlay idles out.
    let end = Instant::now() + Duration::from_secs(20);
    let mut last = u64::MAX;
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(1)).await;
        last = owned_memory(r.pid, r.metrics);
        if last <= limit {
            break;
        }
    }
    assert!(
        last <= limit,
        "idle resident set {} MiB exceeds {} MiB (baseline {} MiB)",
        last >> 20,
        limit >> 20,
        baseline >> 20
    );
}
