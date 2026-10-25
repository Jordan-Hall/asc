//! M31 acceptance: `asc mcp --remote <url>` against an in-process `asc serve` `/mcp` (the real
//! transport, auth layer and handler over a stub backend, behind the static token table; no forge).
//!
//! The server records every request it sees (before auth), so each test can say what crossed the
//! wire: which credential, to which origin, and which calls reached the backend.
#![cfg(feature = "serve")]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::cursor::KeyRing;
use asc_mcp::errors::ToolError;
use asc_mcp::handler::{Handler, HandlerConfig};
use asc_mcp::schema::Surface;
use asc_rpc::auth::{Access, Authorizer, StaticTokens};
use asc_serve::mcp::audit::MemoryAudit;
use asc_serve::mcp::auth::{McpAuth, layer};
use asc_serve::mcp::prm::{metadata_url, scope_layer};
use asc_serve::mcp::{self, HttpPolicy};
use serde_json::{Value, json};
use support::Sandbox;

const READ: &str = "tok-read-0123456789";
const WRITE: &str = "tok-write-0123456789";
const WAIT: Duration = Duration::from_secs(20);

// ------------------------------------------------------------------ the server

/// One request as the server saw it.
#[derive(Debug, Clone)]
struct Seen {
    authorization: Option<String>,
    mcp_method: Option<String>,
    mcp_name: Option<String>,
    mcp_repo: Option<String>,
    extra: Option<String>,
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Seen>>);

impl Recorder {
    fn all(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }
}

async fn record(State(rec): State<Arc<Recorder>>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
    rec.0.lock().unwrap().push(Seen {
        authorization: get("authorization"),
        mcp_method: get("mcp-method"),
        mcp_name: get("mcp-name"),
        mcp_repo: get("mcp-param-repo"),
        extra: get("x-asc-test-extra"),
    });
    next.run(req).await
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// `read_file` answers at once; with `path: "slow"` it reports progress and then never finishes.
struct Stub {
    calls: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl McpBackend for Stub {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(p) = &ctx.progress {
            p.report(1.0, Some(2.0), Some("working"));
        }
        if args.pointer("/path").and_then(Value::as_str) == Some("slow") {
            let _guard = DropCount(self.dropped.clone());
            futures::future::pending::<()>().await;
        }
        let who = ctx.principal.clone().unwrap_or_default();
        Ok(Reply::Messages(Box::pin(futures::stream::iter(vec![Ok(
            json!({ "tool": tool, "principal": who }),
        )]))))
    }
}

/// A running HTTP app on loopback; dropping it stops it.
struct Running {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start(build: impl FnOnce(SocketAddr) -> Router + Send + 'static) -> Self {
        let (addr_tx, addr_rx) = channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let app = build(addr);
                addr_tx.send(addr).unwrap();
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self { addr: addr_rx.recv().unwrap(), stop: Some(stop), thread: Some(thread) }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        // The server may hold a hung call open; do not wait for it.
        drop(self.thread.take());
    }
}

struct Server {
    http: Running,
    seen: Arc<Recorder>,
    calls: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Server {
    fn start() -> Self {
        let seen = Arc::new(Recorder::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (s2, c2, d2) = (seen.clone(), calls.clone(), dropped.clone());
        let http = Running::start(move |addr| {
            let public = format!("http://{addr}");
            let policy = HttpPolicy::new(addr, Some(&public), &[], &[]);
            let handler = Handler::new(
                Stub { calls: c2, dropped: d2 },
                HandlerConfig::new(Surface::Http),
                KeyRing::random().unwrap(),
            );
            let authz: Arc<dyn Authorizer> = Arc::new(
                StaticTokens::new()
                    .with_token(READ, "reader", Access::Read)
                    .with_token(WRITE, "writer", Access::Write),
            );
            let app = scope_layer(
                layer(
                    mcp::router(handler, policy),
                    McpAuth::new(authz.clone(), Arc::new(MemoryAudit::default()), &metadata_url(&public)),
                ),
                authz,
            );
            app.layer(axum::middleware::from_fn_with_state(s2, record))
        });
        Self { http, seen, calls, dropped }
    }

    fn url(&self) -> String {
        self.http.url()
    }
}

// ------------------------------------------------------------------ the client

struct Proxy {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Receiver<String>,
}

fn spawn_proxy(sb: &Sandbox, args: &[&str], token: Option<&str>, loopback_http: bool) -> Proxy {
    let mut cmd: Command = sb.asc(sb.tmp.path());
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = token {
        cmd.env("ASC_TOKEN", t);
    }
    if !loopback_http {
        cmd.env_remove("ASC_ALLOW_LOOPBACK_HTTP");
    }
    let mut child = cmd.spawn().expect("spawn asc");
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let (etx, stderr) = channel();
    std::thread::spawn(move || {
        for l in BufReader::new(err).lines().map_while(Result::ok) {
            if etx.send(l).is_err() {
                break;
            }
        }
    });
    let stdin = child.stdin.take();
    Proxy { child, stdin, lines, stderr }
}

impl Proxy {
    fn send(&mut self, v: &Value) {
        let s = self.stdin.as_mut().expect("stdin open");
        writeln!(s, "{v}").unwrap();
        s.flush().unwrap();
    }

    fn recv(&self) -> Value {
        let l = self.lines.recv_timeout(WAIT).expect("a line from asc mcp --remote");
        serde_json::from_str(&l).unwrap_or_else(|_| panic!("not JSON: {l}"))
    }

    fn silent(&self) -> bool {
        self.lines.recv_timeout(Duration::from_millis(700)).is_err()
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.recv()
    }

    /// Closes standard input and returns the exit status and the whole of standard error.
    fn finish(mut self) -> (std::process::ExitStatus, String) {
        drop(self.stdin.take());
        let status = wait(&mut self.child);
        let mut err = String::new();
        while let Ok(l) = self.stderr.recv_timeout(Duration::from_millis(300)) {
            err.push_str(&l);
            err.push('\n');
        }
        (status, err)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait(child: &mut Child) -> std::process::ExitStatus {
    let end = std::time::Instant::now() + WAIT;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(std::time::Instant::now() < end, "asc mcp --remote did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn log_args(changes: &str) -> Value {
    json!({ "name": "read_file", "arguments": { "repo": "acme/widgets", "path": changes } })
}

fn tool_names(reply: &Value) -> Vec<String> {
    reply["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {reply}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

/// A `credential.helper` that logs every call and answers `get` from `answerN` files (the last
/// one repeats), so a test can hand out a bad token first and a good one later.
struct Helper {
    dir: std::path::PathBuf,
}

fn sh_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

impl Helper {
    fn install(sb: &Sandbox, answers: &[&str]) -> Self {
        let dir = sb.dir("helper");
        let script = dir.join("h.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ndir=\"$(dirname \"$0\")\"\ninput=\"$(cat)\"\n\
             { echo \"== $1\"; echo \"$input\"; echo '--'; } >> \"$dir/log\"\n\
             if [ \"$1\" = get ]; then\n  n=$(cat \"$dir/n\" 2>/dev/null || echo 0)\n  n=$((n+1))\n  echo $n > \"$dir/n\"\n  \
             f=\"$dir/answer$n\"\n  [ -f \"$f\" ] || f=\"$dir/answer_last\"\n  cat \"$f\"\nfi\n",
        )
        .unwrap();
        for (i, a) in answers.iter().enumerate() {
            let body = format!("username=u\npassword={a}\n");
            std::fs::write(dir.join(format!("answer{}", i + 1)), &body).unwrap();
            std::fs::write(dir.join("answer_last"), &body).unwrap();
        }
        std::fs::write(
            sb.user.join(".gitconfig"),
            format!("[credential]\n\thelper = !sh '{}'\n", sh_path(&script)),
        )
        .unwrap();
        Self { dir }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default().replace("\r\n", "\n")
    }

    fn ops(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter_map(|l| l.strip_prefix("== "))
            .map(str::to_owned)
            .collect()
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn discover_list_and_calls_forward_both_ways_without_workspace_tools() {
    let server = Server::start();
    let sb = Sandbox::new();
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], Some(WRITE), true);

    let discover = p.call(1, "server/discover", json!({}));
    assert_eq!(discover["id"], 1);
    assert!(discover["result"].is_object(), "{discover}");

    let list = p.call(2, "tools/list", json!({}));
    let names = tool_names(&list);
    assert!(names.contains(&"read_file".to_owned()), "{names:?}");
    // `status` is a workspace tool: it exists on stdio only.
    assert!(!names.contains(&"status".to_owned()), "{names:?}");
    for n in &names {
        let def = asc_mcp::catalogue::TOOLS.iter().find(|d| d.name == *n && d.on(Surface::Http));
        assert!(def.is_some(), "{n} is not an HTTP tool");
    }

    let call = p.call(3, "tools/call", log_args("c1"));
    assert_eq!(call["id"], 3);
    assert_eq!(call["result"]["structuredContent"]["tool"], "read_file", "{call}");
    assert_eq!(call["result"]["structuredContent"]["principal"], "writer", "{call}");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);

    // Every request carried the token and the routing headers of the body.
    let seen = server.seen.all();
    assert!(seen.iter().all(|s| s.authorization.as_deref() == Some(&format!("Bearer {WRITE}"))), "{seen:?}");
    let last = seen.last().unwrap();
    assert_eq!(last.mcp_method.as_deref(), Some("tools/call"));
    assert_eq!(last.mcp_name.as_deref(), Some("read_file"));
    assert_eq!(last.mcp_repo.as_deref(), Some("acme/widgets"));

    // A notification gets no reply.
    p.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    assert!(p.silent());
    let (status, err) = p.finish();
    assert!(status.success(), "{err}");
    assert!(!err.contains(WRITE), "the token must not be logged: {err}");
}

#[test]
fn progress_arrives_before_the_result() {
    let server = Server::start();
    let sb = Sandbox::new();
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], Some(READ), true);
    let mut params = log_args("c1");
    params["_meta"] = json!({ "progressToken": "pt-1" });
    p.send(&json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": params }));
    let first = p.recv();
    assert_eq!(first["method"], "notifications/progress", "{first}");
    assert_eq!(first["params"]["progressToken"], "pt-1");
    let second = p.recv();
    assert_eq!(second["id"], 7, "{second}");
    assert_eq!(second["result"]["structuredContent"]["tool"], "read_file");
}

#[test]
fn cancellation_closes_the_request_and_the_server_stops_the_call() {
    let server = Server::start();
    let sb = Sandbox::new();
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], Some(READ), true);
    p.send(&json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": log_args("slow") }));
    // The stub is inside the call once it has counted it.
    let end = std::time::Instant::now() + WAIT;
    while server.calls.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < end, "the call never reached the backend");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.dropped.load(Ordering::SeqCst), 0);
    p.send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 9 } }));
    let end = std::time::Instant::now() + WAIT;
    while server.dropped.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < end, "the server never saw the cancellation");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(p.silent(), "a cancelled request gets no reply");
    // The session still works.
    let again = p.call(10, "tools/call", log_args("c1"));
    assert_eq!(again["id"], 10);
}

#[test]
fn credentials_come_from_asc_token_else_the_git_credential_helper() {
    let server = Server::start();
    let sb = Sandbox::new();
    let helper = Helper::install(&sb, &[READ]);

    // ASC_TOKEN wins and the helper is never asked.
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], Some(WRITE), true);
    assert_eq!(p.call(1, "tools/call", log_args("c1"))["result"]["structuredContent"]["principal"], "writer");
    drop(p);
    assert!(helper.ops().is_empty(), "{:?}", helper.ops());

    // Without it the helper is asked for the server host, and told it worked afterwards.
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], None, true);
    let reply = p.call(1, "tools/call", log_args("c1"));
    assert_eq!(reply["result"]["structuredContent"]["principal"], "reader", "{reply}");
    let _ = p.finish();
    assert_eq!(helper.ops(), ["get", "store"], "{}", helper.log());
    let host = format!("host={}", server.http.addr);
    assert!(helper.log().contains(&host), "{}", helper.log());
}

#[test]
fn a_401_rejects_and_asks_the_helper_once_more_then_succeeds_or_fails_with_the_hint() {
    let server = Server::start();
    let host = server.http.addr.to_string();

    // A bad token first, a good one on the re-ask: the request is retried and succeeds.
    let sb = Sandbox::new();
    let helper = Helper::install(&sb, &["bad-token-0000000000", READ]);
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], None, true);
    let reply = p.call(1, "tools/call", log_args("c1"));
    assert_eq!(reply["result"]["structuredContent"]["principal"], "reader", "{reply}");
    let _ = p.finish();
    assert_eq!(helper.ops(), ["get", "erase", "get", "store"], "{}", helper.log());

    // A bad token every time: one re-ask, then the session fails with the hint.
    let sb = Sandbox::new();
    let helper = Helper::install(&sb, &["bad-token-0000000000"]);
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], None, true);
    let reply = p.call(1, "tools/call", log_args("c1"));
    let msg = reply["error"]["message"].as_str().unwrap_or_else(|| panic!("{reply}"));
    assert!(msg.contains(&format!("run git credential approve for {host}")), "{msg}");
    let (status, err) = p.finish();
    assert!(!status.success());
    assert!(err.contains(&format!("run git credential approve for {host}")), "{err}");
    assert_eq!(helper.ops(), ["get", "erase", "get", "erase"], "{}", helper.log());
    assert_eq!(server.calls.load(Ordering::SeqCst), 1, "only the good call reached the backend");

    // No credential at all: the same hint.
    let sb = Sandbox::new();
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], None, true);
    let reply = p.call(1, "tools/list", json!({}));
    assert!(reply["error"]["message"].as_str().unwrap().contains("git credential approve"), "{reply}");
    let (status, _) = p.finish();
    assert!(!status.success());
}

#[test]
fn the_credential_never_goes_to_a_redirect_target_on_another_origin() {
    let evil_seen = Arc::new(Recorder::default());
    let e2 = evil_seen.clone();
    let evil = Running::start(move |_| {
        Router::new()
            .fallback(|| async { StatusCode::OK })
            .layer(axum::middleware::from_fn_with_state(e2, record))
    });
    let target = format!("{}/mcp", evil.url());
    let red_seen = Arc::new(Recorder::default());
    let r2 = red_seen.clone();
    let redirector = Running::start(move |_| {
        Router::new().fallback(move || {
            let target = target.clone();
            async move {
                let mut r = Response::new(axum::body::Body::empty());
                *r.status_mut() = StatusCode::TEMPORARY_REDIRECT;
                r.headers_mut().insert(header::LOCATION, HeaderValue::from_str(&target).unwrap());
                r
            }
        })
        .layer(axum::middleware::from_fn_with_state(r2, record))
    });
    let sb = Sandbox::new();
    // A custom header is not "sensitive" to an HTTP client, so only the origin binding stops it.
    std::fs::write(
        sb.user.join(".gitconfig"),
        "[http]\n\textraHeader = X-Asc-Test-Extra: do-not-leak\n",
    )
    .unwrap();
    let mut p = spawn_proxy(&sb, &["mcp", "--remote", &redirector.url()], Some(WRITE), true);
    let reply = p.call(1, "tools/call", log_args("c1"));
    assert!(reply["error"]["message"].as_str().unwrap().contains("redirect"), "{reply}");
    drop(p);
    // The credential and the header did go to the origin that was asked...
    let asked = red_seen.all();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].extra.as_deref(), Some("do-not-leak"));
    assert_eq!(asked[0].authorization.as_deref(), Some(&*format!("Bearer {WRITE}")));
    // ...and nothing followed the redirect.
    let leaked = evil_seen.all();
    assert!(leaked.is_empty(), "the redirect target was contacted: {leaked:?}");
}

#[test]
fn read_only_and_toolsets_narrow_the_list_and_hidden_calls_never_leave() {
    let server = Server::start();
    let sb = Sandbox::new();

    let mut full = spawn_proxy(&sb, &["mcp", "--remote", &server.url()], Some(WRITE), true);
    let all = tool_names(&full.call(1, "tools/list", json!({ "repo": "acme/widgets" })));
    assert!(all.contains(&"commit".to_owned()) || all.iter().any(|n| {
        asc_mcp::catalogue::TOOLS.iter().any(|d| d.name == *n && d.toolset == asc_mcp::catalogue::Toolset::Write)
    }), "a writer sees a write tool: {all:?}");
    drop(full);

    let mut ro = spawn_proxy(&sb, &["mcp", "--remote", &server.url(), "--read-only"], Some(WRITE), true);
    let names = tool_names(&ro.call(1, "tools/list", json!({ "repo": "acme/widgets" })));
    assert!(!names.is_empty());
    assert!(names.iter().all(|n| {
        asc_mcp::catalogue::TOOLS
            .iter()
            .find(|d| d.name == *n && d.on(Surface::Http))
            .is_some_and(|d| d.toolset != asc_mcp::catalogue::Toolset::Write)
    }), "{names:?}");
    let before = server.calls.load(Ordering::SeqCst);
    let write_tool = all
        .iter()
        .find(|n| !names.contains(n))
        .expect("a write tool that read-only hides");
    let reply = ro.call(2, "tools/call", json!({ "name": write_tool, "arguments": { "repo": "acme/widgets" } }));
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert_eq!(server.calls.load(Ordering::SeqCst), before, "a hidden tool never reaches the server");
    // The refused call was not even sent.
    assert!(!server.seen.all().iter().any(|s| s.mcp_name.as_deref() == Some(write_tool)));
    drop(ro);

    let mut only = spawn_proxy(&sb, &["mcp", "--remote", &server.url(), "--tools", "read_file"], Some(READ), true);
    assert_eq!(tool_names(&only.call(1, "tools/list", json!({}))), ["read_file"]);
    let reply = only.call(2, "tools/call", json!({ "name": "ls_tree", "arguments": { "repo": "acme/widgets" } }));
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
}

#[test]
fn plain_http_and_malformed_urls_are_refused_before_any_credential_is_read() {
    let sb = Sandbox::new();
    let helper = Helper::install(&sb, &[READ]);
    for (url, loopback) in [
        ("http://example.com", true),
        ("http://127.0.0.1:9", false),
        ("https://user:pw@example.com", true),
        ("https://example.com/?token=x", true),
        ("ftp://example.com", true),
        ("not a url", true),
    ] {
        let p = spawn_proxy(&sb, &["mcp", "--remote", url], Some(WRITE), loopback);
        let (status, err) = p.finish();
        assert!(!status.success(), "{url}");
        assert!(err.contains("asc mcp") || err.contains("--remote"), "{url}: {err}");
        assert!(!err.contains("pw") || !err.contains("user:pw"), "{url}: {err}");
    }
    assert!(helper.ops().is_empty());
}
