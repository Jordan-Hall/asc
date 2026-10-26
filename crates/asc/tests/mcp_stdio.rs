//! M09 acceptance: `asc mcp` over stdio against a local-only fixture repository (SPEC 22.5, 22.7.2).
//!
//! The client is a plain newline-delimited JSON-RPC peer over the child's pipes (the inspector
//! style of the PLAN row, without an rmcp client). Run with `ASC_NO_DAEMON=1`, the in-process
//! route; `daemon_route_starts_the_daemon` covers the daemon route on a private endpoint.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use support::{Sandbox, text};

const SECRET: &str = "TOP-SECRET-TOKEN-0451";

fn git_env(c: &mut Command) -> &mut Command {
    c.env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

impl Repo {
    fn asc(&self, args: &[&str]) -> String {
        let mut c = self.sb.asc(&self.root);
        git_env(&mut c).args(args).env("ASC_NO_DAEMON", "1");
        let out = c.output().expect("spawn asc");
        assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }
}

/// A repository with two commits, an ignored `.env`, a directory of five files and a binary file.
fn repo() -> Repo {
    let sb = Sandbox::new();
    let root = sb.dir("repo");
    let r = Repo { sb, root };
    r.asc(&["init", "-q", "."]);
    write(&r.root, ".ascignore", b".env\nbuild/\n");
    write(&r.root, ".env", SECRET.as_bytes());
    write(&r.root, "f.txt", b"alpha\nbeta\ngamma\n");
    write(&r.root, "src/lib.rs", b"pub fn needle() {}\n");
    for i in 1..=5 {
        write(
            &r.root,
            &format!("many/f{i}.txt"),
            format!("file {i}\n").as_bytes(),
        );
    }
    write(&r.root, "bin.dat", b"\x00\x01\x02binary");
    r.asc(&["add", "-A"]);
    r.asc(&["commit", "-q", "-m", "first commit"]);
    write(&r.root, "f.txt", b"alpha\nbeta changed\ngamma\ndelta\n");
    r.asc(&["add", "f.txt"]);
    r.asc(&["commit", "-q", "-m", "second commit"]);
    r
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    err: Arc<Mutex<Vec<u8>>>,
    next: u64,
    /// Every line the server wrote to standard output.
    lines: Vec<String>,
}

impl Server {
    fn start(r: &Repo, extra_env: &[(&str, &str)], daemon: bool) -> Self {
        let mut c = r.sb.asc(&r.root);
        git_env(&mut c)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !daemon {
            c.env("ASC_NO_DAEMON", "1");
        }
        for (k, v) in extra_env {
            c.env(k, v);
        }
        let mut child = c.spawn().expect("spawn asc mcp");
        let pid = child.id().to_string();
        std::thread::spawn(move || {
            // A server that stops answering fails the test instead of hanging it.
            std::thread::sleep(Duration::from_secs(120));
            let _ = if cfg!(windows) {
                Command::new("taskkill").args(["/F", "/PID", &pid]).output()
            } else {
                Command::new("kill").args(["-9", &pid]).output()
            };
        });
        let stdin = child.stdin.take();
        let out = BufReader::new(child.stdout.take().unwrap());
        let err = Arc::new(Mutex::new(Vec::new()));
        let mut stderr = child.stderr.take().unwrap();
        let sink = err.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            stdin,
            out,
            err,
            next: 0,
            lines: Vec::new(),
        }
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.out.read_line(&mut line).expect("read stdout");
        assert!(n > 0, "the server closed stdout; stderr: {}", self.stderr());
        self.lines.push(line.clone());
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let v = self.recv();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// The `tools/call` result (or the JSON-RPC error object).
    fn call(&mut self, name: &str, args: Value) -> Value {
        let v = self.request("tools/call", json!({ "name": name, "arguments": args }));
        if v.get("error").is_some() {
            v
        } else {
            v["result"].clone()
        }
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args.clone());
        assert_eq!(r["isError"], json!(false), "{name} {args}: {r}");
        r["structuredContent"].clone()
    }

    fn error_code(&mut self, name: &str, args: Value) -> String {
        let r = self.call(name, args.clone());
        assert_eq!(r["isError"], json!(true), "{name} {args} should fail: {r}");
        r["structuredContent"]["error"]["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.err.lock().unwrap()).into_owned()
    }

    fn finish(mut self) -> (Vec<String>, String) {
        drop(self.stdin.take());
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(100));
        (self.lines.clone(), self.stderr())
    }
}

fn tool_names(s: &mut Server) -> Vec<String> {
    let r = s.request("tools/list", json!({}));
    r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn discover_and_list_serve_only_read_tools() {
    let r = repo();
    let mut s = Server::start(&r, &[], false);
    let d = s.request("server/discover", json!({}));
    assert!(
        d["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28"))
    );
    let list = s.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "status",
        "diff",
        "log",
        "show",
        "read_file",
        "ls_tree",
        "grep",
        "blame",
        "smartlog",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for t in tools {
        assert_eq!(
            t["annotations"]["readOnlyHint"],
            json!(true),
            "{}",
            t["name"]
        );
    }
    for write in ["create", "modify", "commit", "submit", "stage", "undo"] {
        assert!(
            !names.contains(&write),
            "{write} must not be listed without asc.mcp.write"
        );
    }
    let (_, err) = s.finish();
    assert!(!err.contains(SECRET));
}

#[test]
fn write_config_in_the_repository_cannot_list_write_tools() {
    let r = repo();
    r.asc(&["config", "--local", "asc.mcp.write", "true"]);
    let mut s = Server::start(&r, &[], false);
    let names = tool_names(&mut s);
    assert!(
        !names.iter().any(|n| n == "create" || n == "commit"),
        "{names:?}"
    );
    s.finish();
}

#[test]
fn status_diff_log_show_blame_smartlog() {
    let r = repo();
    write(
        &r.root,
        "f.txt",
        b"alpha\nbeta changed\ngamma\ndelta\nepsilon\n",
    );
    write(&r.root, "new.txt", b"untracked\n");
    let mut s = Server::start(&r, &[], false);

    let st = s.ok("status", json!({}));
    let files = st["files"].as_array().unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"f.txt"), "{st}");
    assert!(
        !paths.contains(&".env"),
        "ignored files are never listed: {st}"
    );
    assert_eq!(st["head_ref"], json!("refs/heads/master"));

    let d = s.ok("diff", json!({}));
    assert!(
        d["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == json!("f.txt")),
        "{d}"
    );

    let l = s.ok("log", json!({}));
    let entries = l["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{l}");
    assert_eq!(
        entries[0]["message"].as_str().unwrap().trim(),
        "second commit"
    );

    let show = s.ok("show", json!({ "rev": "HEAD" }));
    assert_eq!(show["header"]["message"], json!("second commit"));
    assert_eq!(show["header"]["author"]["name"], json!("Test"));
    assert_eq!(show["header"]["parents"].as_array().unwrap().len(), 1);
    assert_eq!(show["changes"], json!([{ "path": "f.txt", "status": "M" }]));
    let root_show = s.ok("show", json!({ "rev": "HEAD~1" }));
    assert!(root_show["changes"].as_array().unwrap().len() > 5);

    let b = s.ok("blame", json!({ "path": "f.txt", "rev": "HEAD" }));
    assert!(!b["hunks"].as_array().unwrap().is_empty(), "{b}");
    let sl = s.ok("smartlog", json!({}));
    assert_eq!(sl["trunks"][0]["name"], json!("master"), "{sl}");
    s.finish();
}

#[test]
fn read_file_ranges_revisions_and_binaries() {
    let r = repo();
    let mut s = Server::start(&r, &[], false);

    let all = s.ok("read_file", json!({ "path": "f.txt" }));
    assert_eq!(all["data"], json!("alpha\nbeta changed\ngamma\ndelta\n"));
    assert_eq!(all["total_lines"], json!(4));
    assert_eq!(all["truncated"], json!(false));

    let ranged = s.ok(
        "read_file",
        json!({ "path": "f.txt", "start_line": 2, "end_line": 3 }),
    );
    assert_eq!(ranged["data"], json!("beta changed\ngamma\n"));
    assert_eq!(ranged["start_line"], json!(2));
    assert_eq!(ranged["end_line"], json!(3));

    let old = s.ok("read_file", json!({ "path": "f.txt", "rev": "HEAD~1" }));
    assert_eq!(old["data"], json!("alpha\nbeta\ngamma\n"));

    let bin = s.ok("read_file", json!({ "path": "bin.dat" }));
    assert_eq!(bin["binary"], json!(true));
    assert!(bin.get("data").is_none());

    // A long file pages at 400 lines; the reply says so and the next call continues.
    let long: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
    write(&r.root, "long.txt", long.as_bytes());
    let p1 = s.ok("read_file", json!({ "path": "long.txt" }));
    assert_eq!(p1["truncated"], json!(true));
    assert_eq!(p1["end_line"], json!(400));
    let p2 = s.ok(
        "read_file",
        json!({ "path": "long.txt", "start_line": 401 }),
    );
    assert_eq!(p2["start_line"], json!(401));
    assert!(p2["data"].as_str().unwrap().starts_with("line 401\n"));
    assert_eq!(
        s.error_code("read_file", json!({ "path": "nope.txt" })),
        "NOT_FOUND"
    );
    s.finish();
}

#[test]
fn ls_tree_pages_and_grep_searches() {
    let r = repo();
    let mut s = Server::start(&r, &[], false);

    let top = s.ok("ls_tree", json!({}));
    let names: Vec<&str> = top["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"f.txt") && names.contains(&"many") && names.contains(&"src"),
        "{names:?}"
    );

    let p1 = s.ok("ls_tree", json!({ "path": "many", "limit": 2 }));
    assert_eq!(p1["entries"].as_array().unwrap().len(), 2);
    assert_eq!(p1["truncated"], json!(true));
    let last = p1["entries"][1]["path"].as_str().unwrap().to_owned();
    let p2 = s.ok(
        "ls_tree",
        json!({ "path": "many", "limit": 10, "after": last }),
    );
    let rest: Vec<&str> = p2["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(rest, ["f3.txt", "f4.txt", "f5.txt"]);
    assert!(p2.get("truncated").is_none());
    assert_eq!(
        s.error_code("ls_tree", json!({ "recursive": true })),
        "INVALID_ARGUMENT"
    );

    let g = s.ok("grep", json!({ "pattern": "needle", "rev": "HEAD" }));
    assert_eq!(
        g["matches"],
        json!([{ "path": "src/lib.rs", "line": 1, "text": "pub fn needle() {}" }])
    );
    let none = s.ok(
        "grep",
        json!({ "pattern": "no-such-text-anywhere", "rev": "HEAD" }),
    );
    assert!(
        none.get("matches")
            .is_none_or(|m| m.as_array().unwrap().is_empty())
    );
    let limited = s.ok(
        "grep",
        json!({ "pattern": "file", "fixed": true, "max_results": 2 }),
    );
    assert_eq!(limited["matches"].as_array().unwrap().len(), 2);
    assert_eq!(limited["truncated"], json!(true));
    s.finish();
}

#[test]
fn refused_paths_never_reach_the_engine() {
    let r = repo();
    let outside = r.sb.dir("outside");
    write(&outside, "secret.txt", SECRET.as_bytes());
    let mut links = Vec::new();
    #[cfg(windows)]
    {
        let junction = r.root.join("junc");
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {}", text(&made.stderr));
        links.push("junc/secret.txt");
        if std::os::windows::fs::symlink_file(outside.join("secret.txt"), r.root.join("flink"))
            .is_ok()
        {
            links.push("flink");
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, r.root.join("junc")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), r.root.join("flink")).unwrap();
        links.extend(["junc/secret.txt", "flink"]);
    }
    let mut s = Server::start(&r, &[], false);

    for tool in ["read_file", "blame"] {
        for bad in [
            "../outside/secret.txt",
            "a/../../x",
            "/etc/passwd",
            "C:/Windows/win.ini",
            "\\\\srv\\share\\x",
            "a\\b",
            "x\0y",
        ] {
            assert_eq!(
                s.error_code(tool, json!({ "path": bad })),
                "PATH_POLICY",
                "{tool} {bad:?}"
            );
        }
        for link in &links {
            assert_eq!(
                s.error_code(tool, json!({ "path": link })),
                "PATH_POLICY",
                "{tool} {link}"
            );
        }
        assert_eq!(
            s.error_code(tool, json!({ "path": ".env" })),
            "PATH_POLICY",
            "{tool} .env"
        );
    }
    assert_eq!(
        s.error_code("grep", json!({ "pattern": "x", "paths": ["../outside"] })),
        "PATH_POLICY"
    );
    assert_eq!(
        s.error_code("ls_tree", json!({ "path": "../outside" })),
        "PATH_POLICY"
    );
    assert_eq!(
        s.error_code("status", json!({ "paths": [".env"] })),
        "PATH_POLICY"
    );
    assert_eq!(
        s.error_code("read_file", json!({ "path": ".env", "rev": "HEAD" })),
        "PATH_POLICY"
    );
    // A search never returns a file the ignore rules hide, even when a revision holds it.
    let found = s.ok("grep", json!({ "pattern": "TOP-SECRET", "fixed": true }));
    assert!(
        found
            .get("matches")
            .is_none_or(|m| m.as_array().unwrap().is_empty()),
        "{found}"
    );

    // A worktree that is not one of this repository's is refused.
    let r2 = s.request(
        "tools/call",
        json!({ "name": "status", "arguments": { "worktree": outside.to_string_lossy() } }),
    );
    assert_eq!(r2["error"]["code"], json!(-32602), "{r2}");
    let own = s.ok("status", json!({ "worktree": r.root.to_string_lossy() }));
    assert_eq!(own["head_ref"], json!("refs/heads/master"));

    let (lines, err) = s.finish();
    assert!(
        !lines.iter().any(|l| l.contains(SECRET)),
        "the secret leaked on the protocol channel"
    );
    assert!(!err.contains(SECRET), "the secret leaked on stderr");
}

#[test]
fn a_stray_print_does_not_break_the_stream() {
    if !cfg!(debug_assertions) {
        return;
    }
    let r = repo();
    let mut s = Server::start(&r, &[("ASC_MCP_TEST_STRAY_STDOUT", "1")], false);
    let d = s.request("server/discover", json!({}));
    assert!(d["result"]["supportedVersions"].is_array());
    let st = s.ok("status", json!({}));
    assert!(st["head"].is_string());
    let (lines, err) = s.finish();
    for l in &lines {
        let v: Value =
            serde_json::from_str(l).unwrap_or_else(|_| panic!("non JSON on stdout: {l:?}"));
        assert_eq!(v["jsonrpc"], json!("2.0"), "{l}");
    }
    assert!(
        err.contains("stray line from in-process code"),
        "the stray print must land on stderr: {err}"
    );
}

#[test]
fn daemon_route_starts_the_daemon() {
    let r = repo();
    let endpoint = if cfg!(windows) {
        format!(
            "asc-m09-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    } else {
        r.sb.dir("ipc")
            .join("d.sock")
            .to_string_lossy()
            .into_owned()
    };
    struct StopDaemon<'a>(&'a Repo, String);
    impl Drop for StopDaemon<'_> {
        fn drop(&mut self) {
            let mut c = self.0.sb.asc(&self.0.root);
            let _ = c
                .args(["daemon", "stop"])
                .env("ASC_DAEMON_ENDPOINT", &self.1)
                .output();
        }
    }
    let _stop = StopDaemon(&r, endpoint.clone());
    let mut s = Server::start(&r, &[("ASC_DAEMON_ENDPOINT", &endpoint)], true);
    let st = s.ok("status", json!({}));
    assert_eq!(st["head_ref"], json!("refs/heads/master"), "{st}");
    let l = s.ok("log", json!({}));
    assert_eq!(l["entries"].as_array().unwrap().len(), 2);
    // Reads that have no daemon RPC still work beside it.
    let f = s.ok("read_file", json!({ "path": "f.txt", "rev": "HEAD~1" }));
    assert_eq!(f["data"], json!("alpha\nbeta\ngamma\n"));
    s.finish();
}

/// I21: `search` is an opt-in toolset that is listed only while the server `origin` names
/// advertises `search.v1`. With no origin, or one that cannot be reached, it is not listed, and the
/// start does not wait on the network for long.
#[test]
fn search_is_not_listed_without_a_server_that_advertises_it() {
    let r = repo();
    r.asc(&["config", "--local", "asc.mcp.toolsets", "core,search"]);
    let mut s = Server::start(&r, &[], false);
    let names = tool_names(&mut s);
    assert!(names.iter().any(|n| n == "grep"), "{names:?}");
    assert!(!names.iter().any(|n| n == "search"), "{names:?}");
    // A call to an unlisted tool is refused like an unknown one.
    let denied = s.call("search", json!({ "query": "needle", "mode": "text" }));
    assert!(denied.get("error").is_some(), "{denied}");
    s.finish();

    r.asc(&[
        "config",
        "--local",
        "remote.origin.url",
        "https://127.0.0.1:1/acme/app",
    ]);
    let started = std::time::Instant::now();
    let mut s = Server::start(&r, &[], false);
    let names = tool_names(&mut s);
    assert!(!names.iter().any(|n| n == "search"), "{names:?}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "start waited too long"
    );
    s.finish();
}

// ---- search (I21) -----------------------------------------------------------------------------

/// An `asc.v1.CodeIntel` stand-in on loopback that also reports its features in `Repo.Version`.
mod codeintel {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use bytes::Bytes;
    use asc_rpc::auth::StaticTokens;
    use asc_rpc::server::{RepoHandlers, RepoService, ServerInfo, wire};
    use asc_rpc::services::code_intel_server::{CodeIntel, CodeIntelServer};
    use tonic::{Request, Response, Status};

    #[derive(Default)]
    pub struct Seen {
        pub search: Vec<wire::SearchRequest>,
        pub auth: Vec<Option<String>>,
    }

    #[derive(Clone)]
    struct Fake(Arc<Mutex<Seen>>);

    type SearchStream =
        futures::stream::Iter<std::vec::IntoIter<Result<wire::SearchResponse, Status>>>;

    fn unused<T>() -> Result<T, Status> {
        Err(Status::unimplemented("not part of this test"))
    }

    #[tonic::async_trait]
    impl CodeIntel for Fake {
        type SearchStream = SearchStream;

        async fn search(
            &self,
            r: Request<wire::SearchRequest>,
        ) -> Result<Response<Self::SearchStream>, Status> {
            let auth = r
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let mut seen = self.0.lock().unwrap();
            seen.auth.push(auth);
            seen.search.push(r.into_inner());
            let hit = |path: &'static [u8]| wire::FileHit {
                path: Bytes::from_static(path),
                matches: vec![wire::LineMatch {
                    line: 1,
                    text: Bytes::from_static(b"pub fn needle() {}"),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let resp = wire::SearchResponse {
                scope: Some(wire::ResolvedScope {
                    changeset: Bytes::from_static(&[0xab, 0xcd]),
                    coverage: 0.5,
                    ..Default::default()
                }),
                // `.env` is ignored by the work tree's rules: the tool must not show it.
                files: vec![hit(b"src/lib.rs"), hit(b".env")],
                ..Default::default()
            };
            Ok(Response::new(futures::stream::iter(vec![Ok(resp)])))
        }

        async fn symbols(
            &self,
            _: Request<wire::SymbolsRequest>,
        ) -> Result<Response<wire::SymbolsResponse>, Status> {
            unused()
        }

        async fn semantic(
            &self,
            _: Request<wire::SemanticRequest>,
        ) -> Result<Response<wire::SemanticResponse>, Status> {
            unused()
        }

        async fn upload_scip(
            &self,
            _: Request<tonic::Streaming<wire::UploadScipRequest>>,
        ) -> Result<Response<wire::UploadScipResponse>, Status> {
            unused()
        }

        async fn index_status(
            &self,
            _: Request<wire::IndexStatusRequest>,
        ) -> Result<Response<wire::IndexStatusResponse>, Status> {
            unused()
        }
    }

    pub struct Server {
        pub addr: SocketAddr,
        pub seen: Arc<Mutex<Seen>>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        pub fn start(features: &[&str]) -> Self {
            let seen = Arc::new(Mutex::new(Seen::default()));
            let fake = Fake(seen.clone());
            let info = ServerInfo {
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                ..ServerInfo::default()
            };
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
                    let repo = RepoService::new(
                        Arc::new(RepoHandlers::new()),
                        Arc::new(StaticTokens::new()),
                        info,
                    );
                    let _ = tonic::transport::Server::builder()
                        .add_service(CodeIntelServer::new(fake))
                        .add_service(repo.into_service())
                        .serve_with_incoming_shutdown(incoming, async {
                            let _ = stop_rx.await;
                        })
                        .await;
                });
            });
            Self {
                addr: addr_rx.recv().unwrap(),
                seen,
                stop: Some(stop),
                thread: Some(thread),
            }
        }

        pub fn url(&self) -> String {
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
}

#[test]
fn search_is_served_by_the_origin_that_advertises_search_v1() {
    use asc_rpc::server::wire;

    let r = repo();
    let server = codeintel::Server::start(&["search.v1"]);
    r.asc(&["config", "--local", "asc.mcp.toolsets", "core,search"]);
    r.asc(&["config", "--local", "remote.origin.url", &server.url()]);
    let mut s = Server::start(&r, &[("ASC_TOKEN", "origin-token")], false);
    let names = tool_names(&mut s);
    assert!(names.iter().any(|n| n == "search"), "{names:?}");

    let found = s.ok("search", json!({ "query": "needle", "mode": "text" }));
    // The reply contract: scope, coverage and an estimate on every mode.
    assert_eq!(found["coverage"], json!(0.5), "{found}");
    assert!(found["est_tokens"].as_u64().unwrap() > 0, "{found}");
    assert_eq!(found["scope"]["changeset"], json!("abcd"), "{found}");
    // The work tree's rules hide `.env`; the other hit stays.
    let files = found["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{found}");
    assert_eq!(files[0]["path"], json!("src/lib.rs"));
    assert!(!found.to_string().contains(".env"));

    // The request: the connection's repository, the default branch, the credential.
    let seen = server.seen.lock().unwrap();
    let q = &seen.search[0];
    assert_eq!(q.pattern, "needle");
    let scope = q.scope.as_ref().unwrap();
    assert_eq!(scope.repo.as_ref().unwrap().name, "acme/app");
    assert!(matches!(
        &scope.at,
        Some(wire::scope::At::Rev(r)) if r == "HEAD"
    ));
    assert_eq!(seen.auth[0].as_deref(), Some("Bearer origin-token"));
    drop(seen);

    // A path outside the repository never leaves the process.
    let code = s.error_code(
        "search",
        json!({ "query": "needle", "mode": "text", "paths": ["../x"] }),
    );
    assert_eq!(code, "PATH_POLICY");
    assert_eq!(server.seen.lock().unwrap().search.len(), 1);
    s.finish();
}

#[test]
fn search_is_not_listed_when_the_origin_lacks_search_v1() {
    let r = repo();
    let server = codeintel::Server::start(&[]);
    r.asc(&["config", "--local", "asc.mcp.toolsets", "core,search"]);
    r.asc(&["config", "--local", "remote.origin.url", &server.url()]);
    let mut s = Server::start(&r, &[("ASC_TOKEN", "origin-token")], false);
    let names = tool_names(&mut s);
    assert!(!names.iter().any(|n| n == "search"), "{names:?}");
    s.finish();
}
