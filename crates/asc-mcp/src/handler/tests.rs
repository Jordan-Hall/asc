use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

use futures::stream;
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::Semaphore;

use super::*;
use crate::catalogue::WriteLevel;
use crate::config::Flags;

type Script = Box<dyn Fn(&str, &Value, &CallContext) -> Result<Reply, ToolError> + Send + Sync>;

struct Mock {
    script: Script,
    worktrees: Vec<PathBuf>,
    started: AtomicUsize,
    /// When set, `call` waits for a permit before answering.
    hold: Option<Arc<Semaphore>>,
    seen_position: Mutex<Option<Position>>,
}

struct M(Arc<Mock>);

#[async_trait::async_trait]
impl McpBackend for M {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        self.0.started.fetch_add(1, Ordering::SeqCst);
        *self.0.seen_position.lock().unwrap() = ctx.position.clone();
        if let Some(h) = &self.0.hold {
            h.acquire().await.unwrap().forget();
        }
        (self.0.script)(tool, args, ctx)
    }

    async fn worktrees(&self) -> Result<Vec<PathBuf>, ToolError> {
        Ok(self.0.worktrees.clone())
    }
}

fn mock(script: impl Fn(&str, &Value, &CallContext) -> Result<Reply, ToolError> + Send + Sync + 'static) -> Arc<Mock> {
    Arc::new(Mock {
        script: Box::new(script),
        worktrees: vec![],
        started: AtomicUsize::new(0),
        hold: None,
        seen_position: Mutex::new(None),
    })
}

fn messages(items: Vec<Value>) -> Result<Reply, ToolError> {
    Ok(Reply::Messages(Box::pin(stream::iter(items.into_iter().map(Ok)))))
}

const KEY: [u8; 32] = [7; 32];

fn handler(m: &Arc<Mock>, cfg: McpConfig) -> Handler<M> {
    let mut hc = HandlerConfig::new(Surface::Stdio);
    hc.config = cfg;
    Handler::new(M(Arc::clone(m)), hc, KeyRing::from_keys(&[KEY]).unwrap())
}

fn writable(level: WriteLevel, confirm: ConfirmMode) -> McpConfig {
    McpConfig { write: level, confirm, ..McpConfig::default() }
}

async fn rpc(h: &Handler<M>, method: &str, params: Value) -> Value {
    let (tx, _rx) = mpsc::channel(8);
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    h.handle_message(msg, &Caller::default(), &tx).await.expect("a response")
}

async fn call(h: &Handler<M>, tool: &str, args: Value) -> Value {
    rpc(h, "tools/call", json!({ "name": tool, "arguments": args })).await
}

fn rpc_code(resp: &Value) -> i64 {
    resp["error"]["code"].as_i64().unwrap_or_else(|| panic!("expected a JSON-RPC error, got {resp}"))
}

fn tool_names(resp: &Value) -> Vec<String> {
    resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn discover_lists_the_json_profile() {
    let h = handler(&mock(|_, _, _| messages(vec![])), McpConfig::default());
    let r = rpc(&h, "server/discover", json!({})).await;
    let result = &r["result"];
    assert!(result["features"].as_array().unwrap().iter().any(|f| f == JSON_PROFILE));
    assert_eq!(result["profiles"][0], "json.profile.v1");
    assert_eq!(result["supportedVersions"][0], "2026-07-28");
    assert!(result["instructions"].as_str().unwrap().contains("data"));
    // Compatibility with hosts that still send `initialize`.
    let r = rpc(&h, "initialize", json!({ "protocolVersion": "2025-11-25" })).await;
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(rpc_code(&rpc(&h, "nope/nothing", json!({})).await), i64::from(METHOD_NOT_FOUND));
}

#[tokio::test]
async fn tools_list_is_read_only_by_default_and_follows_the_write_level() {
    let m = mock(|_, _, _| messages(vec![]));
    let names = tool_names(&rpc(&handler(&m, McpConfig::default()), "tools/list", json!({})).await);
    assert_eq!(names.len(), 11);
    assert!(!names.contains(&"stage".to_owned()));
    let names = tool_names(&rpc(&handler(&m, writable(WriteLevel::Local, ConfirmMode::Destructive)), "tools/list", json!({})).await);
    assert!(names.contains(&"stage".to_owned()) && !names.contains(&"submit".to_owned()));
    // An explicit tool list narrows.
    let cfg = McpConfig { tools: Some(BTreeSet::from(["log".to_owned()])), ..McpConfig::default() };
    assert_eq!(tool_names(&rpc(&handler(&m, cfg), "tools/list", json!({})).await), ["log"]);
}

#[tokio::test]
async fn a_tool_that_is_not_listed_cannot_be_called() {
    let m = mock(|_, _, _| messages(vec![json!({})]));
    // Read-only: a write tool is -32602, the same as a tool that does not exist.
    let h = handler(&m, McpConfig::default());
    let hidden = call(&h, "stage", json!({ "add": ["a"] })).await;
    let missing = call(&h, "no_such_tool", json!({})).await;
    assert_eq!(rpc_code(&hidden), i64::from(INVALID_PARAMS));
    assert_eq!(rpc_code(&missing), i64::from(INVALID_PARAMS));
    // `--read-only` beats a write level.
    let cfg = McpConfig { read_only: true, ..writable(WriteLevel::Remote, ConfirmMode::Never) };
    assert_eq!(rpc_code(&call(&handler(&m, cfg), "stage", json!({ "add": ["a"] })).await), i64::from(INVALID_PARAMS));
    // A toolset that is off hides its tools.
    assert_eq!(rpc_code(&call(&h, "interdiff", json!({})).await), i64::from(INVALID_PARAMS));
    assert_eq!(m.started.load(Ordering::SeqCst), 0, "the backend was never reached");
}

#[tokio::test]
async fn untrusted_config_cannot_open_the_write_gate() {
    let m = mock(|_, _, _| messages(vec![json!({})]));
    let entries = [crate::config::Entry::new(crate::config::Layer::Repo, "write", "remote")];
    let h = handler(&m, McpConfig::resolve(&entries, &Flags::default()));
    assert_eq!(rpc_code(&call(&h, "stage", json!({ "add": ["a"] })).await), i64::from(INVALID_PARAMS));
}

#[tokio::test]
async fn arguments_are_validated() {
    let m = mock(|_, _, _| messages(vec![json!({})]));
    let h = handler(&m, McpConfig::default());
    for bad in [
        json!({ "limit": "ten" }),
        json!({ "surprise": 1 }),
        json!({ "max_tokens": -1 }),
        json!({ "cursor": 5 }),
    ] {
        assert_eq!(rpc_code(&call(&h, "log", bad.clone()).await), i64::from(INVALID_PARAMS), "{bad}");
    }
    let r = rpc(&h, "tools/call", json!({ "name": "log", "arguments": [1] })).await;
    assert_eq!(rpc_code(&r), i64::from(INVALID_PARAMS));
    let r = rpc(&h, "tools/call", json!({ "arguments": {} })).await;
    assert_eq!(rpc_code(&r), i64::from(INVALID_PARAMS));
    assert_eq!(m.started.load(Ordering::SeqCst), 0);
    // Valid arguments reach the backend.
    let ok = call(&h, "log", json!({ "limit": 5 })).await;
    assert_eq!(ok["result"]["isError"], false);
}

#[tokio::test]
async fn a_streamed_reply_is_folded_into_one_message() {
    let m = mock(|_, _, _| messages(vec![json!({ "commits": [1], "a": "x" }), json!({ "commits": [2, 3], "a": "y" })]));
    let r = call(&handler(&m, McpConfig::default()), "log", json!({})).await;
    let res = &r["result"];
    assert_eq!(res["isError"], false);
    assert_eq!(res["structuredContent"], json!({ "commits": [1, 2, 3], "a": "y" }));
    assert_eq!(res["content"][0]["type"], "text");
    assert!(res["content"][0]["text"].as_str().unwrap().contains("commits"));
}

#[tokio::test]
async fn exit_codes_one_and_two_are_results_and_128_is_an_error() {
    let m = mock(|_, args, _| {
        let code = args["add"][0].as_str().unwrap().parse::<i32>().unwrap();
        Ok(Reply::Verb { exit_code: code, result: json!({ "conflicts": ["f.txt"], "candidates": ["a", "b"] }) })
    });
    let h = handler(&m, writable(WriteLevel::Local, ConfirmMode::Never));
    let paused = call(&h, "stage", json!({ "add": ["1"] })).await;
    assert_eq!(paused["result"]["isError"], false);
    assert_eq!(paused["result"]["structuredContent"]["status"], "paused");
    assert_eq!(paused["result"]["structuredContent"]["conflicts"][0], "f.txt");
    let amb = call(&h, "stage", json!({ "add": ["2"] })).await;
    assert_eq!(amb["result"]["isError"], false);
    assert_eq!(amb["result"]["structuredContent"]["status"], "ambiguous");
    for c in ["128", "129"] {
        let r = call(&h, "stage", json!({ "add": [c] })).await;
        assert_eq!(r["result"]["isError"], true, "{c}");
        assert!(r["result"]["structuredContent"]["error"]["code"].is_string());
    }
}

#[tokio::test]
async fn backend_errors_become_error_results_with_redacted_text() {
    let m = mock(|_, _, _| {
        Err(ToolError::new("NOT_FOUND", "no repo at https://bob:hunter2@host.example/r.git").with_paths(vec!["a".into()]))
    });
    let r = call(&handler(&m, McpConfig::default()), "log", json!({})).await;
    let res = &r["result"];
    assert_eq!(res["isError"], true);
    assert_eq!(res["structuredContent"]["error"]["code"], "NOT_FOUND");
    assert_eq!(res["structuredContent"]["error"]["paths"][0], "a");
    assert!(!r.to_string().contains("hunter2"));
}

#[tokio::test]
async fn destructive_calls_fail_closed_without_confirmation() {
    let m = mock(|_, _, _| Ok(Reply::Verb { exit_code: 0, result: json!({ "status": "ok" }) }));
    let args = json!({ "op": "abc" });
    let h = handler(&m, writable(WriteLevel::Local, ConfirmMode::Destructive));
    let r = call(&h, "undo", args.clone()).await;
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(r["result"]["structuredContent"]["error"]["code"], "NEEDS_CONFIRMATION");
    assert_eq!(m.started.load(Ordering::SeqCst), 0, "the backend never ran");
    // A non-destructive write needs no confirmation.
    assert_eq!(call(&h, "stage", json!({ "add": ["a"] })).await["result"]["isError"], false);
    // The user's switch.
    let h = handler(&m, writable(WriteLevel::Local, ConfirmMode::Never));
    assert_eq!(call(&h, "undo", args).await["result"]["isError"], false);
}

#[test]
fn confirmation_rules_follow_the_catalogue() {
    let def = |n| catalogue::find(n, Surface::Stdio).unwrap();
    let m = ConfirmMode::Destructive;
    assert!(needs_confirmation(def("submit"), &json!({}), m));
    assert!(!needs_confirmation(def("submit"), &json!({}), ConfirmMode::Never));
    assert!(needs_confirmation(def("sync"), &json!({ "delete_merged": true }), m));
    assert!(!needs_confirmation(def("sync"), &json!({ "delete_merged": false }), m));
    assert!(needs_confirmation(def("undo"), &json!({}), m));
    assert!(!needs_confirmation(def("stage"), &json!({}), ConfirmMode::Always));
    let rebase = catalogue::find("rebase", Surface::Http).unwrap();
    assert!(!needs_confirmation(rebase, &json!({ "update_ref": "refs/heads/x" }), m), "dry run by default");
    assert!(needs_confirmation(rebase, &json!({ "update_ref": "refs/heads/x", "dry_run": false }), m));
    let commit = catalogue::find("commit", Surface::Http).unwrap();
    assert!(needs_confirmation(commit, &json!({ "update_ref": "refs/for/master" }), m));
    assert!(!needs_confirmation(commit, &json!({ "update_ref": "refs/heads/x" }), m));
}

#[test]
fn update_ref_of_commit_and_rebase_is_empty_or_refs_for() {
    for tool in ["commit", "rebase"] {
        let def = catalogue::find(tool, Surface::Http).unwrap();
        for ok in [json!({}), json!({ "update_ref": "" }), json!({ "update_ref": "refs/for/master" })] {
            assert!(check_update_ref(def, &ok).is_ok(), "{tool} {ok}");
        }
        for bad in ["master", "refs/heads/master", "refs/for/", "refs/for/../heads/m", "refs/for//x", "refs/tags/v"] {
            let e = check_update_ref(def, &json!({ "update_ref": bad })).unwrap_err();
            assert_eq!(e.code, INVALID_PARAMS, "{tool} {bad}");
        }
        assert!(check_update_ref(def, &json!({ "update_ref": 5 })).is_err());
    }
    // Other tools are not touched.
    let restack = catalogue::find("restack", Surface::Http).unwrap();
    assert!(check_update_ref(restack, &json!({ "update_ref": "refs/heads/x" })).is_ok());
}

fn long_items(n: usize) -> Vec<Value> {
    (0..n).map(|i| json!({ "commits": [format!("{i}{}", "x".repeat(100))] })).collect()
}

#[tokio::test]
async fn the_budget_cuts_a_stream_and_a_cursor_continues_it() {
    let m = mock(|_, _, _| messages(long_items(10)));
    let h = handler(&m, McpConfig::default());
    let r = call(&h, "log", json!({ "max_tokens": 200 })).await;
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(sc["truncated"], true);
    let kept = sc["commits"].as_array().unwrap().len();
    assert!((1..10).contains(&kept), "kept {kept}");
    let cursor = sc["next_cursor"].as_str().unwrap().to_owned();
    // The continuation reaches the backend as a verified position.
    let again = call(&h, "log", json!({ "max_tokens": 200, "cursor": cursor.clone() })).await;
    assert_eq!(again["result"]["isError"], false);
    assert_eq!(*m.seen_position.lock().unwrap(), Some(Position::Index { index: kept as u64 }));
    // The same cursor with other arguments, or a forged one, is -32602.
    let other = call(&h, "log", json!({ "limit": 3, "cursor": cursor.clone() })).await;
    assert_eq!(rpc_code(&other), i64::from(INVALID_PARAMS));
    let other_tool = call(&h, "show", json!({ "cursor": cursor })).await;
    assert_eq!(rpc_code(&other_tool), i64::from(INVALID_PARAMS));
    assert_eq!(rpc_code(&call(&h, "log", json!({ "cursor": "AAAA" })).await), i64::from(INVALID_PARAMS));
    // A cursor sealed under another process key (stdio keys are per process) is refused.
    let foreign = KeyRing::from_keys(&[[9u8; 32]]).unwrap().seal("log", &json!({}), Position::Index { index: 1 });
    assert_eq!(rpc_code(&call(&h, "log", json!({ "cursor": foreign })).await), i64::from(INVALID_PARAMS));
}

#[tokio::test]
async fn one_oversize_message_is_a_budget_error() {
    let m = mock(|_, _, _| messages(vec![json!({ "blob": "y".repeat(5000) })]));
    let r = call(&handler(&m, McpConfig::default()), "log", json!({ "max_tokens": 100 })).await;
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(r["result"]["structuredContent"]["error"]["code"], "BUDGET");
}

#[tokio::test]
async fn max_tokens_cannot_exceed_the_configured_budget() {
    let m = mock(|_, _, ctx| messages(vec![json!({ "budget": ctx.budget_bytes })]));
    let h = handler(&m, McpConfig::default());
    let r = call(&h, "log", json!({ "max_tokens": 25_000 })).await;
    assert_eq!(r["result"]["structuredContent"]["budget"], 6000 * 3);
}

#[tokio::test]
async fn worktree_must_be_a_listed_worktree() {
    let listed = std::env::temp_dir();
    let other = std::env::current_dir().unwrap();
    assert_ne!(std::fs::canonicalize(&listed).unwrap(), std::fs::canonicalize(&other).unwrap());
    let m = Arc::new(Mock {
        script: Box::new(|_, _, _| messages(vec![json!({})])),
        worktrees: vec![listed.clone()],
        started: AtomicUsize::new(0),
        hold: None,
        seen_position: Mutex::new(None),
    });
    let h = handler(&m, McpConfig::default());
    let ok = call(&h, "status", json!({ "worktree": listed.join(".").to_string_lossy() })).await;
    assert_eq!(ok["result"]["isError"], false, "{ok}");
    for bad in [other.to_string_lossy().into_owned(), "no/such/dir".to_owned(), "..".to_owned()] {
        let r = call(&h, "status", json!({ "worktree": bad })).await;
        assert_eq!(rpc_code(&r), i64::from(INVALID_PARAMS), "{bad}");
    }
    assert_eq!(m.started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn progress_is_reported_when_a_token_is_set() {
    let m = mock(|_, _, ctx| {
        ctx.progress.as_ref().expect("a progress sink").report(1.0, Some(2.0), Some("fetching"));
        messages(vec![json!({})])
    });
    let h = handler(&m, McpConfig::default());
    let (tx, mut rx) = mpsc::channel(8);
    let msg = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "log", "arguments": {}, "_meta": { "progressToken": "p1" } } });
    let resp = h.handle_message(msg, &Caller::default(), &tx).await.unwrap();
    assert_eq!(resp["result"]["isError"], false);
    let n = rx.try_recv().unwrap();
    assert_eq!(n["method"], "notifications/progress");
    assert_eq!(n["params"]["progressToken"], "p1");
    assert_eq!(n["params"]["total"], 2.0);
}

#[tokio::test]
async fn eight_calls_run_thirty_two_queue_and_the_next_is_busy() {
    let hold = Arc::new(Semaphore::new(0));
    let m = Arc::new(Mock {
        script: Box::new(|_, _, _| messages(vec![json!({ "ok": true })])),
        worktrees: vec![],
        started: AtomicUsize::new(0),
        hold: Some(Arc::clone(&hold)),
        seen_position: Mutex::new(None),
    });
    let h = handler(&m, McpConfig::default());
    let mut tasks = Vec::new();
    for i in 0..(MAX_INFLIGHT + MAX_QUEUE) {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let (tx, _rx) = mpsc::channel(8);
            let msg = json!({ "jsonrpc": "2.0", "id": i, "method": "tools/call", "params": { "name": "log", "arguments": {} } });
            h.handle_message(msg, &Caller::default(), &tx).await.unwrap()
        }));
    }
    wait_until(|| m.started.load(Ordering::SeqCst) == MAX_INFLIGHT).await;
    // Give the other 32 time to queue.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(m.started.load(Ordering::SeqCst), MAX_INFLIGHT, "only 8 run at once");
    let busy = call(&h, "log", json!({})).await;
    assert_eq!(busy["result"]["isError"], true);
    assert_eq!(busy["result"]["structuredContent"]["error"]["message"], "busy");
    assert_eq!(busy["result"]["structuredContent"]["error"]["code"], "RATE_LIMITED");
    hold.add_permits(1000);
    for t in tasks {
        let r = t.await.unwrap();
        assert_eq!(r["result"]["isError"], false, "queued calls complete: {r}");
    }
    // Slots are free again.
    assert_eq!(call(&h, "log", json!({})).await["result"]["isError"], false);
}

async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached in 5 s");
}

struct Client {
    w: WriteHalf<DuplexStream>,
    r: BufReader<ReadHalf<DuplexStream>>,
}

impl Client {
    async fn send(&mut self, v: Value) {
        self.w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.w.write_all(bytes).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.r.read_line(&mut line))
            .await
            .expect("a message within 5 s")
            .unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    }
}

fn connect(h: &Handler<M>) -> (Client, tokio::task::JoinHandle<io::Result<()>>) {
    let (c, s) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(s);
    let (cr, cw) = tokio::io::split(c);
    let h = h.clone();
    let task = tokio::spawn(async move { h.serve(BufReader::new(sr), sw, Caller::default()).await });
    (Client { w: cw, r: BufReader::new(cr) }, task)
}

#[tokio::test]
async fn the_duplex_transport_serves_discover_and_calls() {
    let m = mock(|_, _, _| messages(vec![json!({ "ok": true })]));
    let h = handler(&m, McpConfig::default());
    let (mut c, task) = connect(&h);
    c.send(json!({ "jsonrpc": "2.0", "id": "a", "method": "server/discover" })).await;
    let r = c.recv().await;
    assert_eq!(r["id"], "a");
    assert!(r["result"]["features"].as_array().unwrap().iter().any(|f| f == "json.profile.v1"));
    c.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "log" } })).await;
    assert_eq!(c.recv().await["result"]["structuredContent"]["ok"], true);
    // Garbage, a batch and a non-object do not stop the server.
    c.send_raw(b"{not json\n").await;
    assert_eq!(c.recv().await["error"]["code"], PARSE_ERROR);
    c.send(json!([{ "jsonrpc": "2.0", "id": 9, "method": "ping" }])).await;
    assert_eq!(c.recv().await["error"]["code"], INVALID_REQUEST);
    c.send(json!({ "jsonrpc": "2.0", "id": 10, "method": "ping" })).await;
    assert_eq!(c.recv().await["id"], 10);
    drop(c);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_oversize_message_is_refused_and_skipped() {
    let m = mock(|_, _, _| messages(vec![]));
    let (mut c, task) = connect(&handler(&m, McpConfig::default()));
    let big = vec![b'a'; MAX_MESSAGE_BYTES + 10];
    c.send_raw(&big).await;
    c.send_raw(b"\n").await;
    assert_eq!(c.recv().await["error"]["code"], INVALID_REQUEST);
    c.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })).await;
    assert_eq!(c.recv().await["id"], 1);
    drop(c);
    task.await.unwrap().unwrap();
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn never_ending(dropped: &Arc<AtomicBool>) -> Result<Reply, ToolError> {
    let guard = DropFlag(Arc::clone(dropped));
    Ok(Reply::Messages(Box::pin(stream::poll_fn(move |_| {
        let _keep = &guard;
        Poll::<Option<Result<Value, ToolError>>>::Pending
    }))))
}

#[tokio::test]
async fn cancelled_notification_drops_the_stream_and_sends_no_response() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d = Arc::clone(&dropped);
    let m = mock(move |_, _, _| never_ending(&d));
    let (mut c, task) = connect(&handler(&m, McpConfig::default()));
    c.send(json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "log", "arguments": {} } })).await;
    wait_until(|| m.started.load(Ordering::SeqCst) == 1).await;
    assert!(!dropped.load(Ordering::SeqCst));
    c.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 7 } })).await;
    wait_until(|| dropped.load(Ordering::SeqCst)).await;
    // The next response on the wire is the ping's: call 7 got none.
    c.send(json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" })).await;
    assert_eq!(c.recv().await["id"], 8);
    drop(c);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn closing_the_stream_drops_running_calls() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d = Arc::clone(&dropped);
    let m = mock(move |_, _, _| never_ending(&d));
    let (mut c, task) = connect(&handler(&m, McpConfig::default()));
    c.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "log", "arguments": {} } })).await;
    wait_until(|| m.started.load(Ordering::SeqCst) == 1).await;
    drop(c);
    task.await.unwrap().unwrap();
    assert!(dropped.load(Ordering::SeqCst), "the producer was dropped with the stream");
}
