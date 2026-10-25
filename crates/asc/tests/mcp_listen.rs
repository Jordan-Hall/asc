//! M11 acceptance: `subscriptions/listen` over `asc mcp` stdio, served from the daemon's
//! `Workspace.Watch` on a private endpoint (SPEC 22.3.5).

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{Sandbox, text};

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

struct Rig {
    sb: Sandbox,
    root: PathBuf,
    endpoint: String,
}

impl Rig {
    fn asc(&self, args: &[&str]) -> String {
        let mut c = self.sb.asc(&self.root);
        git_env(&mut c)
            .args(args)
            .env("ASC_DAEMON_ENDPOINT", &self.endpoint);
        let out = c.output().expect("spawn asc");
        assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }
}

impl Rig {
    /// The launch workspace's id, from the plugin API verb the server itself uses.
    fn ws_id(&self) -> String {
        let out = self.asc(&["ws-api", "worktrees"]);
        let line = out.lines().find(|l| !l.trim().is_empty()).unwrap();
        let v: Value = serde_json::from_str(line).unwrap();
        v["worktrees"][0]["ws_id"].as_str().unwrap().to_owned()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let mut c = self.sb.asc(&self.root);
        let _ = c
            .args(["daemon", "stop"])
            .env("ASC_DAEMON_ENDPOINT", &self.endpoint)
            .output();
    }
}

fn rig() -> Rig {
    let sb = Sandbox::new();
    let root = sb.dir("repo");
    let endpoint = if cfg!(windows) {
        format!(
            "asc-m11-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    } else {
        sb.dir("ipc").join("d.sock").to_string_lossy().into_owned()
    };
    let r = Rig { sb, root, endpoint };
    r.asc(&["init", "-q", "."]);
    write(&r.root, ".ascignore", b".env\n");
    write(&r.root, ".env", b"TOP-SECRET\n");
    write(&r.root, "f.txt", b"alpha\n");
    write(&r.root, "g.txt", b"gamma\n");
    r.asc(&["add", "-A"]);
    r.asc(&["commit", "-q", "-m", "first"]);
    r
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next: u64,
}

impl Server {
    fn start(r: &Rig) -> Self {
        let mut c = r.sb.asc(&r.root);
        git_env(&mut c)
            .arg("mcp")
            .env("ASC_DAEMON_ENDPOINT", &r.endpoint)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().expect("spawn asc mcp");
        let pid = child.id().to_string();
        std::thread::spawn(move || {
            // A server that stops answering fails the test instead of hanging it.
            std::thread::sleep(Duration::from_secs(150));
            let _ = if cfg!(windows) {
                Command::new("taskkill").args(["/F", "/PID", &pid]).output()
            } else {
                Command::new("kill").args(["-9", &pid]).output()
            };
        });
        let stdin = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = stderr.read_to_end(&mut sink);
        });
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next: 0,
        }
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn recv_within(&self, d: Duration) -> Option<Value> {
        let l = self.lines.recv_timeout(d).ok()?;
        Some(serde_json::from_str(&l).unwrap_or_else(|e| panic!("not JSON ({e}): {l:?}")))
    }

    /// Sends a request and returns its id.
    fn start_request(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn response(&mut self, id: u64) -> Value {
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let v = self.recv_within(left).expect("the server answered");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start_request(method, params);
        self.response(id)
    }

    /// Whether a `resources/updated` of `uri` arrives within `within`.
    fn updated(&self, uri: &str, within: Duration) -> bool {
        let end = Instant::now() + within;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let Some(v) = self.recv_within(left) else {
                return false;
            };
            if v["method"] == "notifications/resources/updated" && v["params"]["uri"] == uri {
                return true;
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn listen(s: &mut Server, uris: &[String]) -> u64 {
    s.start_request(
        "subscriptions/listen",
        json!({ "resourceSubscriptions": uris }),
    )
}

fn is_error(v: &Value, code: i64) -> bool {
    v["error"]["code"] == json!(code)
}

#[test]
fn head_events_become_resource_updates() {
    let r = rig();
    let ws = r.ws_id();
    let mut s = Server::start(&r);
    let status = format!("asc://ws/{ws}/status");
    let smartlog = format!("asc://ws/{ws}/smartlog");
    let file = format!("asc://ws/{ws}/file/f.txt");
    listen(&mut s, &[status.clone(), smartlog.clone(), file]);
    // The stream opens after the request is read; give it a moment, then move HEAD.
    std::thread::sleep(Duration::from_secs(2));
    write(&r.root, "f.txt", b"alpha changed\n");
    r.asc(&["add", "f.txt"]);
    r.asc(&["commit", "-q", "-m", "second"]);
    assert!(
        s.updated(&status, Duration::from_secs(60)),
        "HEAD moved: status is updated"
    );
}

#[test]
fn bad_and_hidden_uris_and_the_limits_are_refused() {
    let r = rig();
    let ws = r.ws_id();
    let mut s = Server::start(&r);
    for uri in [
        format!("asc://ws/{ws}/file/.env"),
        format!("asc://ws/{ws}/file/..%2Fx"),
        format!("asc://ws/{ws}/file/%2Fetc%2Fpasswd"),
        "asc://ws/other/status".to_owned(),
        format!("asc://{ws}/status"),
    ] {
        let v = s.request(
            "subscriptions/listen",
            json!({ "resourceSubscriptions": [uri] }),
        );
        assert!(is_error(&v, -32602), "{uri}: {v}");
    }
    let many: Vec<String> = (0..65)
        .map(|i| format!("asc://ws/{ws}/file/f{i}.txt"))
        .collect();
    let v = s.request(
        "subscriptions/listen",
        json!({ "resourceSubscriptions": many }),
    );
    assert!(is_error(&v, -32602), "{v}");
    // Two listens open; the third is refused; cancelling one frees its slot.
    let status = format!("asc://ws/{ws}/status");
    let a = listen(&mut s, std::slice::from_ref(&status));
    listen(&mut s, std::slice::from_ref(&status));
    let v = s.request(
        "subscriptions/listen",
        json!({ "resourceSubscriptions": [status.clone()] }),
    );
    assert!(is_error(&v, -32600), "{v}");
    s.send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": a } }));
    std::thread::sleep(Duration::from_millis(500));
    let c = listen(&mut s, &[status]);
    assert!(
        s.recv_within(Duration::from_secs(2)).is_none(),
        "the freed slot took a new listen (request {c}) with no error"
    );
}
