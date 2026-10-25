//! M16 acceptance: `asc mcp` budgets (SPEC 16.4, 22.5). The default `tools/list` estimate runs
//! always; the RSS gates are `#[ignore]` (`cargo test -p asc --test mcp_budgets -- --ignored`) and
//! scale with `ASC_MCP_BUDGET_FILES` (default 2000; the CI job sets the HUGE generator size).
//!
//! RSS is the peak working set / `VmHWM` of the `asc mcp` process itself, read through the OS.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{Sandbox, text};

const MIB: u64 = 1024 * 1024;
const IDLE_RSS_LIMIT: u64 = 24 * MIB;
const LOAD_RSS_LIMIT: u64 = 64 * MIB;
const LIST_TOKEN_LIMIT: usize = 6_000;
const DIFF_TOKENS: u64 = 25_000;
const CONCURRENT: usize = 8;

fn git_env(c: &mut Command) -> &mut Command {
    c.env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
}

fn asc(sb: &Sandbox, root: &Path, args: &[&str]) {
    let mut c = sb.asc(root);
    git_env(&mut c).args(args).env("ASC_NO_DAEMON", "1");
    let out = c.output().expect("spawn asc");
    assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
}

fn body(v: usize, i: usize) -> String {
    (0..40)
        .map(|l| format!("line {l} of file {i} v{v}\n"))
        .collect()
}

fn path(root: &Path, i: usize) -> std::path::PathBuf {
    root.join(format!("d{}/s{}/f{i}.txt", i % 50, i % 7))
}

/// `files` small files committed, then a working tree change big enough to fill many diffs.
fn repo(files: usize) -> (Sandbox, std::path::PathBuf) {
    let sb = Sandbox::new();
    let root = sb.dir("repo");
    asc(&sb, &root, &["init", "-q", "."]);
    for i in 0..files {
        let p = path(&root, i);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body(0, i)).unwrap();
    }
    asc(&sb, &root, &["add", "-A"]);
    asc(&sb, &root, &["commit", "-q", "-m", "base"]);
    // About 4x the per-call byte budget (25k tokens * 3 bytes), so every call fills its budget.
    let want = (DIFF_TOKENS as usize) * 3 * 4;
    let mut total = 0;
    let mut i = 0;
    while total < want && i < files {
        let s = body(1, i);
        total += s.len() * 2;
        std::fs::write(path(&root, i), s).unwrap();
        i += 1;
    }
    (sb, root)
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    next: u64,
}

impl Server {
    fn start(sb: &Sandbox, root: &Path) -> Self {
        let mut c = sb.asc(root);
        git_env(&mut c)
            .arg("mcp")
            .env("ASC_NO_DAEMON", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().expect("spawn asc mcp");
        let pid = child.id().to_string();
        std::thread::spawn(move || {
            // A server that stops answering fails the test instead of hanging it.
            std::thread::sleep(Duration::from_secs(300));
            let _ = if cfg!(windows) {
                Command::new("taskkill").args(["/F", "/PID", &pid]).output()
            } else {
                Command::new("kill").args(["-9", &pid]).output()
            };
        });
        let stdin = child.stdin.take();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            out,
            next: 0,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        let stdin = self.stdin.as_mut().unwrap();
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{req}").unwrap();
        stdin.flush().unwrap();
        id
    }

    fn read_line(&mut self) -> (String, Value) {
        let mut line = String::new();
        assert!(
            self.out.read_line(&mut line).unwrap() > 0,
            "server closed stdout"
        );
        let v: Value = serde_json::from_str(&line).expect("stdout line is JSON");
        (line, v)
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        loop {
            let (_, v) = self.read_line();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn close(mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn default_tools_list_fits_the_token_budget() {
    let (sb, root) = repo(10);
    let mut s = Server::start(&sb, &root);
    let d = s.request("server/discover", json!({}));
    assert!(d.get("error").is_none(), "{d}");
    let list = s.request("tools/list", json!({}));
    let est = list["result"].to_string().len().div_ceil(3); // SPEC 22.6.2: bytes / 3, rounded up
    assert!(est <= LIST_TOKEN_LIMIT, "tools/list estimates {est} tokens");
    s.close();
}

/// Peak resident bytes of `child` (peak working set on Windows, `VmHWM` on Linux).
#[cfg(windows)]
fn peak_rss(child: &Child) -> u64 {
    // The workspace forbids unsafe code, so ask PowerShell for the process's peak working set.
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("(Get-Process -Id {}).PeakWorkingSet64", child.id()))
        .output()
        .expect("spawn powershell");
    text(&out.stdout).trim().parse().expect("PeakWorkingSet64")
}

#[cfg(not(windows))]
fn peak_rss(child: &Child) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{}/status", child.id())).expect("read status");
    let kb: u64 = s
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kb * 1024
}

fn files() -> usize {
    std::env::var("ASC_MCP_BUDGET_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

#[test]
#[ignore = "budget gate: run with --ignored"]
fn idle_rss_stays_within_24_mib() {
    let (sb, root) = repo(files());
    let mut s = Server::start(&sb, &root);
    s.request("server/discover", json!({}));
    s.request("tools/list", json!({}));
    let st = s.request(
        "tools/call",
        json!({"name": "log", "arguments": {"limit": 1}}),
    );
    assert_eq!(st["result"]["isError"], json!(false), "{st}");
    let rss = peak_rss(&s.child);
    eprintln!("idle peak rss: {} MiB", rss / MIB);
    assert!(
        rss <= IDLE_RSS_LIMIT,
        "idle RSS {rss} bytes exceeds {IDLE_RSS_LIMIT}"
    );
    s.close();
}

#[test]
#[ignore = "budget gate: run with --ignored"]
fn eight_concurrent_25k_token_diffs_stay_within_64_mib() {
    let (sb, root) = repo(files());
    let mut s = Server::start(&sb, &root);
    s.request("server/discover", json!({}));
    let t = Instant::now();
    let mut remaining: Vec<u64> = (0..CONCURRENT)
        .map(|_| {
            s.send(
                "tools/call",
                json!({"name": "diff", "arguments": {"max_tokens": DIFF_TOKENS, "max_files": 300}}),
            )
        })
        .collect();
    let mut slowest = Duration::ZERO;
    while !remaining.is_empty() {
        let (line, v) = s.read_line();
        if let Some(pos) = remaining.iter().position(|id| v["id"] == json!(id)) {
            remaining.swap_remove(pos);
            slowest = t.elapsed();
            assert_eq!(v["result"]["isError"], json!(false), "{v}");
            // Bounded: the 25k-token budget (3 bytes per token) plus the JSON envelope.
            let est = line.len().div_ceil(3) as u64;
            assert!(est <= DIFF_TOKENS * 2, "diff result estimates {est} tokens");
        }
    }
    let rss = peak_rss(&s.child);
    eprintln!(
        "load peak rss: {} MiB; {CONCURRENT} diffs in {slowest:?}",
        rss / MIB
    );
    assert!(
        rss <= LOAD_RSS_LIMIT,
        "load RSS {rss} bytes exceeds {LOAD_RSS_LIMIT}"
    );
    s.close();
}
