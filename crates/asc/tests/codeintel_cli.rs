//! I17 acceptance: `asc search`, `asc sym`, `asc semantic` and `asc codeintel status` against a
//! stand-in `asc.v1.CodeIntel` server on loopback. They show the request each command builds,
//! the `git grep -n` layout with the scope header on stderr, the asc JSON profile for `--json`,
//! and that file content from the server cannot reach the terminal as control sequences.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asc_rpc::server::wire;
use asc_rpc::services::code_intel_server::{CodeIntel, CodeIntelServer};
use bytes::Bytes;
use serde_json::Value;
use support::{Sandbox, text};
use tonic::{Request, Response, Status};

const TOKEN: &str = "secret-token-value";

#[derive(Default)]
struct Seen {
    search: Vec<wire::SearchRequest>,
    symbols: Vec<wire::SymbolsRequest>,
    semantic: Vec<wire::SemanticRequest>,
    status: Vec<wire::IndexStatusRequest>,
    /// The header and the concatenated data parts of each `UploadScip` stream.
    uploads: Vec<(wire::UploadScipHeader, Vec<u8>)>,
    auth: Vec<Option<String>>,
}

#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Seen>>,
    /// Return nothing instead of the canned hits.
    empty: bool,
}

fn resolved() -> Option<wire::ResolvedScope> {
    Some(wire::ResolvedScope {
        changeset: Bytes::from_static(&[0xab, 0xcd, 0xef, 0x01, 0x02, 0x03]),
        root: Bytes::new(),
        generation: 9,
        coverage: 0.5,
    })
}

impl Fake {
    fn note<T>(&self, r: &Request<T>) {
        let a = r
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.seen.lock().unwrap().auth.push(a);
    }
}

type SearchStream = futures::stream::Iter<std::vec::IntoIter<Result<wire::SearchResponse, Status>>>;

#[tonic::async_trait]
impl CodeIntel for Fake {
    type SearchStream = SearchStream;

    async fn search(
        &self,
        r: Request<wire::SearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().search.push(r.into_inner());
        let files = if self.empty {
            vec![]
        } else {
            vec![wire::FileHit {
                path: Bytes::from_static(b"src/\x1b]0;owned\x07a.rs"),
                matches: vec![wire::LineMatch {
                    line: 7,
                    text: Bytes::from_static(b"let x = \x1b[2J\x1b[31mneedle\x1b[0m;\r"),
                    before: vec![Bytes::from_static(b"// before")],
                    ..Default::default()
                }],
                ..Default::default()
            }]
        };
        let resp = wire::SearchResponse {
            scope: resolved(),
            files,
            ..Default::default()
        };
        Ok(Response::new(futures::stream::iter(vec![Ok(resp)])))
    }

    async fn symbols(
        &self,
        r: Request<wire::SymbolsRequest>,
    ) -> Result<Response<wire::SymbolsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().symbols.push(r.into_inner());
        Ok(Response::new(wire::SymbolsResponse {
            scope: resolved(),
            symbols: vec![wire::Symbol {
                name: "needle".into(),
                kind: "function".into(),
                is_def: true,
                path: Bytes::from_static(b"src/a.rs"),
                line: 3,
                signature: Bytes::from_static(b"fn needle(\x1b[2J)"),
                confidence: "import_match".into(),
                ..Default::default()
            }],
            ..Default::default()
        }))
    }

    async fn semantic(
        &self,
        r: Request<wire::SemanticRequest>,
    ) -> Result<Response<wire::SemanticResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().semantic.push(r.into_inner());
        Ok(Response::new(wire::SemanticResponse {
            scope: resolved(),
            chunks: vec![wire::Chunk {
                path: Bytes::from_static(b"src/a.rs"),
                start_line: 3,
                end_line: 4,
                text: Bytes::from_static(b"fn needle() {\n}\x1b[2J"),
                ..Default::default()
            }],
            ..Default::default()
        }))
    }

    async fn upload_scip(
        &self,
        r: Request<tonic::Streaming<wire::UploadScipRequest>>,
    ) -> Result<Response<wire::UploadScipResponse>, Status> {
        use wire::upload_scip_request::Part;
        self.note(&r);
        let mut stream = r.into_inner();
        let mut header = None;
        let mut data = Vec::new();
        while let Some(m) = stream.message().await? {
            match m.part {
                Some(Part::Header(h)) if header.is_none() => header = Some(h),
                Some(Part::Data(d)) if header.is_some() => data.extend_from_slice(&d),
                _ => return Err(Status::invalid_argument("bad stream")),
            }
        }
        let header = header.ok_or_else(|| Status::invalid_argument("no header"))?;
        let resp = wire::UploadScipResponse {
            changeset: Bytes::from_static(&[0xab, 0xcd]),
            documents: 2,
            occurrences: data.len() as u32,
            unknown_path: 1,
            ..Default::default()
        };
        self.seen.lock().unwrap().uploads.push((header, data));
        Ok(Response::new(resp))
    }

    async fn index_status(
        &self,
        r: Request<wire::IndexStatusRequest>,
    ) -> Result<Response<wire::IndexStatusResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().status.push(r.into_inner());
        Ok(Response::new(wire::IndexStatusResponse {
            scope: resolved(),
            indexes: vec![wire::IndexStatus {
                index: "trigram".into(),
                coverage: 0.5,
                lag_seconds: 4,
                generation: 9,
                version: "v1".into(),
            }],
        }))
    }
}

struct Server {
    addr: SocketAddr,
    fake: Fake,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(empty: bool) -> Self {
        let fake = Fake {
            empty,
            ..Fake::default()
        };
        let svc = fake.clone();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let incoming = futures::stream::unfold(listener, |l| async move {
                    let r = l.accept().await.map(|(s, _)| s);
                    Some((r, l))
                });
                let _ = tonic::transport::Server::builder()
                    .add_service(CodeIntelServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self {
            addr: addr_rx.recv().unwrap(),
            fake,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("asc+http://{}/acme/app", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(sb: &Sandbox, cwd: &Path, server: &Server, args: &[&str]) -> Output {
    // `--remote` goes before a `--` that ends the options, else it would be a pathspec.
    let split = args.iter().position(|a| *a == "--").unwrap_or(args.len());
    let mut cmd = sb.asc(cwd);
    cmd.env("ASC_TOKEN", TOKEN)
        .env("ASC_NO_DAEMON", "1")
        .args(&args[..split])
        .args(["--remote", &server.url()])
        .args(&args[split..]);
    cmd.output().expect("spawn asc")
}

fn no_controls(s: &str) {
    assert!(
        !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
        "control characters reached the output: {s:?}"
    );
}

#[test]
fn search_prints_git_grep_layout_and_the_scope_header() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "search", "-i", "-C", "1", "--lang", "rust", "needle", "--", "src",
        ],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(so.contains(":7:let x = \\x1b[2J"), "{so:?}");
    assert!(
        so.contains("-6-// before") || so.contains("a.rs-6-"),
        "{so:?}"
    );
    assert!(se.contains("acme/app @ rev master"), "{se}");
    assert!(
        se.contains("abcdef010203") && se.contains("coverage 50%"),
        "{se}"
    );
    assert!(se.contains("incomplete"), "{se}");
    assert!(!se.contains(TOKEN) && !so.contains(TOKEN));

    let seen = server.fake.seen.lock().unwrap();
    let req = &seen.search[0];
    assert_eq!(req.pattern, "needle");
    assert_eq!(req.mode, wire::SearchMode::Literal as i32);
    assert!(!req.case_sensitive);
    assert_eq!(req.context_lines, 1);
    let scope = req.scope.as_ref().unwrap();
    assert_eq!(scope.paths, ["src"]);
    assert_eq!(scope.langs, ["rust"]);
    assert_eq!(scope.repo.as_ref().unwrap().name, "acme/app");
    assert!(matches!(scope.at, Some(wire::scope::At::Rev(ref r)) if r == "master"));
    assert!(seen.auth[0].as_deref().is_some_and(|a| a.contains(TOKEN)));
}

#[test]
fn server_text_cannot_inject_terminal_sequences() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    for args in [
        &["search", "needle"][..],
        &["sym", "def", "needle"],
        &["semantic", "needle"],
        &["codeintel", "status"],
    ] {
        let out = run(&sb, &cwd, &server, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        no_controls(&text(&out.stdout));
        no_controls(&text(&out.stderr));
    }
}

#[test]
fn no_match_exits_one_like_git_grep() {
    let server = Server::start(true);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let out = run(&sb, &cwd, &server, &["search", "-E", "--rev", "v1", "x+"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(text(&out.stdout).is_empty());
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.search[0].mode, wire::SearchMode::Regex as i32);
    assert!(matches!(
        seen.search[0].scope.as_ref().unwrap().at,
        Some(wire::scope::At::Rev(ref r)) if r == "v1"
    ));
}

#[test]
fn json_prints_the_asc_profile_one_object_per_line() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let out = run(&sb, &cwd, &server, &["search", "--json", "needle"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let so = text(&out.stdout);
    let lines: Vec<Value> = so
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{so}");
    // snake_case names, lowercase hex ids, numbers for 64-bit integers.
    assert_eq!(lines[0]["scope"]["changeset"], "abcdef010203");
    assert_eq!(lines[0]["scope"]["generation"], 9);
    assert!(lines[0]["files"][0]["matches"][0]["line"].is_number());

    let out = run(&sb, &cwd, &server, &["codeintel", "status", "--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert_eq!(v["indexes"][0]["lag_seconds"], 4);
}

#[test]
fn sym_and_semantic_and_status_build_their_requests() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "sym",
            "refs",
            "needle",
            "--kind",
            "function",
            "--at",
            "src/a.rs:3",
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with("src/a.rs:3:fn needle("),
        "{}",
        text(&out.stdout)
    );
    let out = run(&sb, &cwd, &server, &["sym", "outline", "src/a.rs"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = run(
        &sb,
        &cwd,
        &server,
        &["semantic", "--vector", "-k", "5", "--rerank", "find needle"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("src/a.rs:3:fn needle() {"));
    let out = run(&sb, &cwd, &server, &["codeintel", "status", "--rev", "v2"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("trigram\tcoverage 50%\tlag 4s\tgeneration 9"));

    let seen = server.fake.seen.lock().unwrap();
    let s = &seen.symbols;
    assert_eq!(s[0].op, wire::SymbolOp::Refs as i32);
    assert_eq!(
        (s[0].def_path.as_ref(), s[0].def_line),
        (&b"src/a.rs"[..], 3)
    );
    assert_eq!(s[0].kinds, ["function"]);
    assert_eq!(s[1].op, wire::SymbolOp::Outline as i32);
    assert_eq!(s[1].path.as_ref(), b"src/a.rs");
    let m = &seen.semantic[0];
    assert_eq!(
        (m.mode, m.k, m.rerank),
        (wire::SemanticMode::Vector as i32, 5, true)
    );
    assert!(matches!(
        seen.status[0].scope.as_ref().unwrap().at,
        Some(wire::scope::At::Rev(ref r)) if r == "v2"
    ));
}

#[test]
fn upload_scip_streams_the_index_in_bounded_parts() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    // Two and a half parts, so the stream has a header and three data messages.
    let index: Vec<u8> = (0..(2 * (1 << 20) + (1 << 19)))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(cwd.join("idx.scip"), &index).unwrap();
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "codeintel",
            "upload-scip",
            "idx.scip",
            "--rev",
            "main",
            "--root",
            "svc",
        ],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(
        so.contains("uploaded 2 documents")
            && so.contains("at abcd")
            && so.contains("1 documents not in the revision"),
        "{so:?}"
    );
    assert!(!se.contains(TOKEN) && !so.contains(TOKEN));
    let seen = server.fake.seen.lock().unwrap();
    let (h, data) = &seen.uploads[0];
    assert_eq!(h.repo.as_ref().unwrap().name, "acme/app");
    assert_eq!((h.rev.as_str(), h.root.as_str()), ("main", "svc"));
    assert_eq!(h.size as usize, index.len());
    assert_eq!(data, &index);
    assert!(seen.auth[0].as_deref().is_some_and(|a| a.contains(TOKEN)));
}

#[test]
fn upload_scip_refuses_what_can_never_be_accepted_before_calling() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    std::fs::write(cwd.join("empty.scip"), b"").unwrap();
    for args in [
        &["codeintel", "upload-scip", "empty.scip"][..],
        &["codeintel", "upload-scip", "missing.scip"],
        &["codeintel", "upload-scip", "empty.scip", "--root", "../x"],
    ] {
        let out = run(&sb, &cwd, &server, args);
        assert!(!out.status.success(), "{args:?} should fail");
    }
    assert!(server.fake.seen.lock().unwrap().uploads.is_empty());
}

#[test]
fn bad_usage_is_refused_before_any_call() {
    let server = Server::start(false);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    for args in [
        &["search", "-E", "-w", "x"][..],
        &["search", "--diff-only", "x"],
        &["search", "--rev", "a", "--stack", "x"],
        &["sym", "bogus", "x"],
        &["sym", "def", "x", "--at", "nolinenumber"],
        &["semantic", "-k", "0", "x"],
    ] {
        let out = run(&sb, &cwd, &server, args);
        assert!(!out.status.success(), "{args:?} should fail");
    }
    let seen = server.fake.seen.lock().unwrap();
    assert!(seen.search.is_empty() && seen.symbols.is_empty() && seen.semantic.is_empty());
}
