//! M10 acceptance: the local write tools of `asc mcp` over stdio (SPEC 22.3.3, 22.5).
//!
//! The client is a plain newline-delimited JSON-RPC peer over the child's pipes (the inspector
//! style of the PLAN row, without an rmcp client). Run with `ASC_NO_DAEMON=1`, the in-process
//! route; `daemon_route_starts_the_daemon` covers the daemon route on a private endpoint.

mod support;

const SECRET: &str = "TOP-SECRET-TOKEN-0451";

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn grant(r: &Repo, level: &str) {
    std::fs::write(
        r.sb.user.join(".gitconfig"),
        format!("[asc \"mcp\"]\n\twrite = {level}\n"),
    )
    .unwrap();
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

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

const WRITES: [&str; 8] = [
    "stage",
    "create",
    "modify",
    "absorb",
    "restack",
    "undo",
    "redo",
    "worktree_create",
];

#[test]
fn write_tools_are_absent_unless_the_user_grants_them() {
    let r = repo();
    // No config and `false` list nothing.
    for setup in [None, Some("false")] {
        if let Some(v) = setup {
            grant(&r, v);
        }
        let mut s = Server::start(&r, &[], false);
        let names = tool_names(&mut s);
        for w in WRITES.iter().chain(&["submit", "sync"]) {
            assert!(!names.iter().any(|n| n == w), "{w} listed with {setup:?}");
        }
        s.finish();
    }
    // A repository-level grant is ignored.
    r.asc(&["config", "--local", "asc.mcp.write", "remote"]);
    let mut s = Server::start(&r, &[], false);
    assert!(!tool_names(&mut s).iter().any(|n| n == "create"));
    s.finish();
    // A user grant lists the local ones.
    grant(&r, "local");
    let mut s = Server::start(&r, &[], false);
    let names = tool_names(&mut s);
    for w in WRITES {
        assert!(names.iter().any(|n| n == w), "{w} missing from {names:?}");
    }
    assert!(!names.iter().any(|n| n == "submit" || n == "sync"));
    s.finish();
    // `--read-only` always wins.
    let mut c = r.sb.asc(&r.root);
    git_env(&mut c)
        .args(["mcp", "--read-only"])
        .env("ASC_NO_DAEMON", "1");
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.contains("\"tools\""), "{line}");
    assert!(!line.contains("\"create\""), "{line}");
    drop(stdin);
    let _ = child.wait();
}

#[test]
fn the_local_remote_split_follows_the_upload_state() {
    let r = repo();
    grant(&r, "local");
    // A configured server with the cloud on uploads drafts: the change verbs need `remote`.
    r.asc(&[
        "config",
        "--local",
        "remote.origin.url",
        "https://example.invalid/x.git",
    ]);
    r.asc(&["config", "--local", "asc.cloud", "on"]);
    let mut s = Server::start(&r, &[], false);
    let names = tool_names(&mut s);
    assert!(names.iter().any(|n| n == "stage") && names.iter().any(|n| n == "worktree_create"));
    for n in ["create", "modify", "restack", "undo"] {
        assert!(
            !names.iter().any(|x| x == n),
            "{n} needs write=remote online: {names:?}"
        );
    }
    s.finish();
    grant(&r, "remote");
    let mut s = Server::start(&r, &[], false);
    let list = s.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let create = tools
        .iter()
        .find(|t| t["name"] == "create")
        .expect("create listed");
    assert_eq!(create["annotations"]["openWorldHint"], json!(true));
    let stage = tools.iter().find(|t| t["name"] == "stage").unwrap();
    assert_eq!(stage["annotations"]["openWorldHint"], json!(false));
    s.finish();
    // Without a remote the same user grant keeps them local.
    r.asc(&["config", "--local", "--unset", "remote.origin.url"]);
    grant(&r, "local");
    let mut s = Server::start(&r, &[], false);
    assert!(tool_names(&mut s).iter().any(|n| n == "create"));
    s.finish();
}

#[test]
fn create_modify_restack_undo_round_trip() {
    let r = repo();
    grant(&r, "local");
    // Changes live off the trunk: `modify` rewrites drafts only.
    r.asc(&["switch", "-q", "-c", "work"]);
    let mut s = Server::start(&r, &[], false);

    write(&r.root, "one.txt", b"one\n");
    r.asc(&["add", "one.txt"]);
    let c = s.ok("create", json!({ "message": "add one", "all": true }));
    assert_eq!(c["status"], json!("ok"), "{c}");
    let create_op = c["op"].as_str().expect("create names its op").to_owned();
    assert_eq!(r.asc(&["log", "-1", "--format=%s"]).trim(), "add one");

    write(&r.root, "one.txt", b"one amended\n");
    let m = s.ok("modify", json!({ "all": true }));
    let modify_op = m["op"].as_str().expect("modify names its op").to_owned();
    assert_ne!(create_op, modify_op);
    assert_eq!(r.asc(&["show", "HEAD:one.txt"]).trim(), "one amended");

    let rs = s.ok("restack", json!({}));
    assert!(
        ["ok", "nothing"].contains(&rs["status"].as_str().unwrap()),
        "{rs}"
    );

    // `undo` is destructive: without an elicitation-capable client it needs the user's
    // `asc.mcp.confirm = never`, which only the user's config can set.
    assert_eq!(
        s.error_code("undo", json!({ "op": create_op })),
        "NEEDS_CONFIRMATION"
    );
    s.finish();
    std::fs::write(
        r.sb.user.join(".gitconfig"),
        "[asc \"mcp\"]
	write = local
	confirm = never
",
    )
    .unwrap();
    let mut s = Server::start(&r, &[], false);
    // The explicit op: an older one is stale, the newest is accepted.
    assert_eq!(s.error_code("undo", json!({ "op": create_op })), "STALE_OP");
    let latest = {
        let log: Value = serde_json::from_str(&r.asc(&["op", "log", "--json", "-n", "1"])).unwrap();
        log["ops"][0]["op"].as_str().unwrap().to_owned()
    };
    let head_before = r.asc(&["rev-parse", "HEAD"]);
    let dry = s.ok("undo", json!({ "op": latest, "dry_run": true }));
    assert!(dry.is_object());
    assert_eq!(
        r.asc(&["rev-parse", "HEAD"]),
        head_before,
        "a dry run moves nothing"
    );
    let u = s.ok("undo", json!({ "op": latest }));
    assert_eq!(u["status"], json!("ok"), "{u}");
    let redo = s.ok("redo", json!({ "op": latest }));
    assert!(redo.is_object());
    s.finish();
}

#[test]
fn input_cannot_become_a_flag() {
    let r = repo();
    grant(&r, "local");
    let mut s = Server::start(&r, &[], false);
    for (tool, args) in [
        ("create", json!({ "message": "m", "onto": "--help" })),
        ("create", json!({ "message": "" })),
        ("undo", json!({ "op": "--force" })),
        ("restack", json!({ "change": "--upstack" })),
    ] {
        let bad = s.call(tool, args.clone());
        assert_eq!(bad["isError"], json!(true), "{tool} {args}: {bad}");
    }
    assert!(r.asc(&["log", "--format=%s"]).starts_with("second commit"));
    s.finish();
}

#[test]
fn a_cancelled_child_leaves_no_index_lock() {
    let r = repo();
    grant(&r, "local");
    // Off the trunk; `.git` is a file that names the real git directory.
    r.asc(&["switch", "-q", "-c", "work"]);
    let dot = std::fs::read_to_string(r.root.join(".git")).unwrap();
    let gitdir = PathBuf::from(dot.trim().strip_prefix("gitdir:").unwrap().trim());
    let common = std::fs::read_to_string(gitdir.join("commondir")).unwrap();
    let hooks = gitdir.join(common.trim()).join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    let lock = gitdir.join("index.lock");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > '{}'\nsleep 120\n",
            lock.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(&r.root, "slow.txt", b"slow\n");
    let mut s = Server::start(&r, &[], false);
    s.next += 1;
    let id = s.next;
    s.send(&json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "create", "arguments": { "message": "slow", "all": true } } }));
    wait_for("the hook's lock", 60, || lock.exists());
    s.send(
        &json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": id } }),
    );
    wait_for("lock recovery", 60, || !lock.exists());
    // The server is still serving.
    let st = s.ok("status", json!({}));
    assert!(st.is_object());
    s.finish();
}

#[test]
fn submit_needs_confirmation() {
    let r = repo();
    grant(&r, "remote");
    let mut s = Server::start(&r, &[], false);
    assert!(tool_names(&mut s).iter().any(|n| n == "submit"));
    // No elicitation capability on this client: the tool refuses instead of running.
    assert_eq!(s.error_code("submit", json!({})), "NEEDS_CONFIRMATION");
    assert_eq!(
        s.error_code("sync", json!({ "delete_merged": true })),
        "NEEDS_CONFIRMATION"
    );
    s.finish();
}

#[test]
fn stage_obeys_the_path_gate() {
    let r = repo();
    grant(&r, "local");
    write(&r.root, "new.txt", b"new\n");
    let mut s = Server::start(&r, &[], false);
    s.ok("stage", json!({ "add": ["new.txt"] }));
    assert!(
        r.asc(&["diff", "--cached", "--name-only"])
            .contains("new.txt")
    );
    assert_eq!(
        s.error_code("stage", json!({ "add": ["../outside"] })),
        "PATH_POLICY"
    );
    assert_eq!(
        s.error_code("stage", json!({ "add": [".env"] })),
        "PATH_POLICY"
    );
    let evil = "diff --git a/../x b/../x\n--- a/../x\n+++ b/../x\n@@ -0,0 +1 @@\n+x\n";
    assert_eq!(
        s.error_code("stage", json!({ "patch": evil })),
        "PATH_POLICY"
    );
    s.finish();
}

#[test]
fn worktree_create_adds_a_worktree() {
    let r = repo();
    grant(&r, "local");
    let mut s = Server::start(&r, &[], false);
    let bad = s.call("worktree_create", json!({ "path": "inner/.git/x" }));
    assert_eq!(bad["isError"], json!(true), "{bad}");
    let ok = s.ok(
        "worktree_create",
        json!({ "path": "../wt-one", "mode": "plain" }),
    );
    assert!(ok.is_object());
    assert!(r.root.parent().unwrap().join("wt-one").is_dir());
    s.finish();
}
