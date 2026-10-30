//! Authentication on demand (docs/asc/FEATURE-ROADMAP.md, "CLI authentication model"): a command
//! starts anonymously and asks for a credential only when the server answers that it needs one.
//!
//! The world is the production shape: a hub in front (here a mock that answers the Forgejo API
//! `asc serve` delegates to, `/.well-known/asc`, and the device sign-in endpoints) and the real
//! `asc serve` behind it, with the real `asc` client in between. A person at a terminal is
//! simulated with `ASC_AUTH_INTERACTIVE=1` (the session counts as interactive though no TTY is
//! attached); without it every run is non-interactive.
#![cfg(feature = "serve")]
#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;
use support::{Sandbox, text};

const ADMIN: &str = "tok-admin-8c1f";
const ALICE: &str = "tok-alice-5d2e";
const BOB: &str = "tok-bob-77aa";
const STALE: &str = "tok-stale-0000";
const ALL_TOKENS: [&str; 4] = [ADMIN, ALICE, BOB, STALE];

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn asc_exe() -> &'static str {
    env!("CARGO_BIN_EXE_asc")
}

fn clean(cmd: &mut Command, home: &Path) {
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy().to_ascii_uppercase();
        if name.starts_with("GIT_") || name.starts_with("ASC_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("ASC_HOME", home.join("asc-home"))
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig-none"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
        .env("NO_COLOR", "1");
}

// ------------------------------------------------------------------------------ the mock hub

#[derive(Default)]
struct HubState {
    /// `METHOD path` of every request, in order.
    log: Vec<String>,
    /// Device polls answered so far.
    polls: usize,
    /// Polls answered `authorization_pending` before the approval.
    pending: usize,
    /// The token a finished device sign-in hands out.
    device_token: String,
}

/// The hub: Forgejo API for `asc serve`, discovery, device sign-in.
struct Hub {
    port: u16,
    state: Arc<Mutex<HubState>>,
}

fn user_of(token: &str) -> Option<(i64, &'static str)> {
    match token {
        ADMIN => Some((1, "root")),
        ALICE => Some((2, "alice")),
        BOB => Some((3, "bob")),
        _ => None,
    }
}

fn respond(conn: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        conn,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// gRPC arrives as HTTP/2 with prior knowledge (it starts with "PRI "): the hub passes those
/// connections through to the server, as the production hub fronts it on one origin.
fn pass_through(client: TcpStream, serve_port: u16) {
    let Ok(server) = TcpStream::connect(("127.0.0.1", serve_port)) else {
        return;
    };
    let (mut c2, mut s2) = (client.try_clone().unwrap(), server.try_clone().unwrap());
    let (mut c, mut s) = (client, server);
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut c2, &mut s2);
        let _ = s2.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut s, &mut c);
    let _ = c.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

fn handle(mut conn: TcpStream, state: &Arc<Mutex<HubState>>, port: u16, serve_port: u16) {
    let mut preface = [0u8; 4];
    if conn.peek(&mut preface).is_ok_and(|n| n == 4) && &preface == b"PRI " {
        return pass_through(conn, serve_port);
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = conn.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let mut len = 0usize;
    for l in lines {
        let lower = l.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    while buf.len() < head_end + 4 + len {
        let n = conn.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let path = path.split('?').next().unwrap_or("").to_owned();
    state.lock().unwrap().log.push(format!("{method} {path}"));
    let base = format!("http://127.0.0.1:{port}/");
    match (method, path.as_str()) {
        ("GET", "/.well-known/asc") => {
            respond(
                &mut conn,
                200,
                &format!("{{\"server\":\"http://127.0.0.1:{port}\"}}"),
            );
        }
        ("POST", "/login/device/code") => respond(
            &mut conn,
            200,
            &format!(
                "{{\"device_code\":\"dc-1\",\"user_code\":\"ABCD-EFGH\",\"verification_uri\":\"{base}login/device\",\"verification_uri_complete\":\"{base}login/device?user_code=ABCD-EFGH\",\"expires_in\":600,\"interval\":1}}"
            ),
        ),
        ("POST", "/login/device/token") => {
            let mut s = state.lock().unwrap();
            s.polls += 1;
            if s.polls <= s.pending {
                respond(&mut conn, 400, "{\"error\":\"authorization_pending\"}");
            } else {
                let body = format!(
                    "{{\"access_token\":\"{}\",\"token_type\":\"bearer\"}}",
                    s.device_token
                );
                respond(&mut conn, 200, &body);
            }
        }
        // The forge's push hooks and event sink accept everything.
        ("POST", p) if p.starts_with("/api/internal/hook/") || p == "/api/internal/asc/event" => {
            respond(&mut conn, 200, "{}");
        }
        ("POST", "/api/internal/asc/authz") => {
            let body: serde_json::Value =
                serde_json::from_slice(&buf[head_end + 4..]).unwrap_or(serde_json::Value::Null);
            let token = body.get("token").and_then(|t| t.as_str());
            let owner = body.get("owner").and_then(|t| t.as_str()).unwrap_or("");
            let name = body.get("repo").and_then(|t| t.as_str()).unwrap_or("");
            let denied = |code: i64| {
                format!("{{\"allowed\":false,\"user_id\":{code},\"username\":\"\",\"perm\":\"\"}}")
            };
            let answer = match token.map(user_of) {
                // A credential Forgejo does not know authenticates nobody.
                Some(None) => denied(0),
                Some(Some((id, login))) if owner.is_empty() => format!(
                    "{{\"allowed\":true,\"user_id\":{id},\"username\":\"{login}\",\"perm\":\"read\"}}"
                ),
                None if owner.is_empty() => denied(0),
                _ if owner != "acme" => denied(token.and_then(user_of).map_or(0, |u| u.0)),
                Some(Some((id, login))) => {
                    let perm = match (login, name) {
                        ("root", _) => Some("admin"),
                        ("alice", _) => Some("write"),
                        (_, "priv") => None,
                        _ => Some("read"),
                    };
                    match perm {
                        Some(p) => format!(
                            "{{\"allowed\":true,\"user_id\":{id},\"username\":\"{login}\",\"perm\":\"{p}\",\"repo_id\":{}}}",
                            if name == "pub" { 101 } else { 102 }
                        ),
                        None => denied(id),
                    }
                }
                None => {
                    // Anonymous: the public repository reads, nothing else exists.
                    if name == "pub" {
                        "{\"allowed\":true,\"user_id\":0,\"username\":\"\",\"perm\":\"read\",\"repo_id\":101}"
                            .to_owned()
                    } else {
                        denied(0)
                    }
                }
            };
            respond(&mut conn, 200, &answer);
        }
        _ => respond(&mut conn, 404, "{}"),
    }
}

impl Hub {
    fn start(port: u16, serve_port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let state = Arc::new(Mutex::new(HubState {
            pending: 1,
            device_token: ALICE.to_owned(),
            ..HubState::default()
        }));
        let s = state.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let s = s.clone();
                std::thread::spawn(move || handle(conn, &s, port, serve_port));
            }
        });
        Self { port, state }
    }

    fn url(&self, repo: &str) -> String {
        format!("http://127.0.0.1:{}/{repo}", self.port)
    }

    fn device_requests(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.contains("/login/device/"))
            .count()
    }

    fn hands_out(&self, token: &str) {
        self.state.lock().unwrap().device_token = token.to_owned();
    }
}

// ------------------------------------------------------------------------------ the world

struct World {
    dir: tempfile::TempDir,
    hub: Hub,
    serve_port: u16,
    child: Option<Child>,
}

impl World {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (hub_port, serve_port, admin_port) = (free_port(), free_port(), free_port());
        let hub = Hub::start(hub_port, serve_port);
        let cfg = dir.path().join("asc-serve.toml");
        std::fs::write(
            &cfg,
            format!(
                "listen = \"127.0.0.1:{serve_port}\"\npublic_url = \"http://127.0.0.1:{serve_port}\"\n\
                 store = \"memory:///\"\n\
                 db = \"sqlite:{db}?mode=rwc\"\n\
                 cache = {{ disk_dir = {cache:?}, disk = \"1GiB\" }}\n\
                 [auth]\nprovider = \"forgejo\"\n\
                 [forgejo]\nurl = \"http://127.0.0.1:{hub_port}\"\n\
                 internal_token_file = {internal:?}\n\
                 [admin]\nlisten = \"127.0.0.1:{admin_port}\"\n",
                db = fwd(&dir.path().join("asc.db")),
                cache = fwd(&dir.path().join("cache")),
                internal = fwd(&dir.path().join("internal.token")),
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("internal.token"), "internal-secret-3f9a\n").unwrap();
        std::fs::create_dir_all(dir.path().join("asc-home")).unwrap();
        let mut w = Self {
            dir,
            hub,
            serve_port,
            child: None,
        };
        let migrate = w.serve(&["migrate"]);
        assert!(
            migrate.status.success(),
            "migrate: {}",
            text(&migrate.stderr)
        );
        let log = std::fs::File::create(w.dir.path().join("serve.log")).unwrap();
        let mut cmd = Command::new(asc_exe());
        clean(&mut cmd, w.dir.path());
        cmd.arg("serve")
            .arg("--config")
            .arg(w.dir.path().join("asc-serve.toml"))
            .current_dir(w.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        w.child = Some(cmd.spawn().expect("spawn asc serve"));
        w.wait_ready();
        w.seed();
        w
    }

    fn serve(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(asc_exe());
        clean(&mut cmd, self.dir.path());
        cmd.arg("serve")
            .arg("--config")
            .arg(self.dir.path().join("asc-serve.toml"))
            .args(args)
            .current_dir(self.dir.path());
        cmd.output().expect("run asc serve")
    }

    fn serve_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("serve.log")).unwrap_or_default()
    }

    fn wait_ready(&mut self) {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.serve_port));
        let end = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(c) = self.child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                panic!("asc serve exited early ({status}): {}", self.serve_log());
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return;
            }
            assert!(
                Instant::now() < end,
                "asc serve never listened: {}",
                self.serve_log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn serve_url(&self, repo: &str) -> String {
        format!("http://127.0.0.1:{}/{repo}", self.serve_port)
    }

    /// Creates `acme/pub` and `acme/priv` and pushes one commit to each, as the administrator.
    fn seed(&self) {
        let sb = Sandbox::new();
        for name in ["pub", "priv"] {
            let repo = format!("acme/{name}");
            let req =
                format!(r#"{{"repo":{{"name":"{repo}"}},"init":{{"default_branch":"main"}}}}"#);
            let mut cmd = Command::new(asc_exe());
            clean(&mut cmd, self.dir.path());
            let out = cmd
                .args([
                    "api",
                    "RepoAdmin",
                    "--remote",
                    &self.serve_url(&repo),
                    "--json",
                    &req,
                ])
                .env("ASC_TOKEN", ADMIN)
                .current_dir(self.dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "RepoAdmin {repo}: {}\nserve: {}",
                text(&out.stderr),
                self.serve_log()
            );
            let work = sb.dir(&format!("seed-{name}"));
            let run = |args: &[&str]| {
                let mut c = sb.asc(&work);
                c.envs(IDENT).env("ASC_TOKEN", ADMIN).args(args);
                let o = c.output().unwrap();
                assert!(o.status.success(), "{args:?}: {}", text(&o.stderr));
            };
            run(&["init", "-q"]);
            run(&["remote", "add", "origin", &self.serve_url(&repo)]);
            std::fs::write(work.join("hello.txt"), format!("{name}\n")).unwrap();
            run(&["add", "hello.txt"]);
            run(&["commit", "-q", "-m", "first"]);
            run(&["branch", "-M", "main"]);
            run(&["push", "origin", "main"]);
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ------------------------------------------------------------------------------ the client

/// One person's machine: a home with an optional `git credential-store` file.
struct Machine {
    sb: Sandbox,
    store: PathBuf,
}

impl Machine {
    /// A home that, when asked, keeps credentials in a `git credential-store` file.
    fn new(_world: &World, with_helper: bool) -> Self {
        let sb = Sandbox::new();
        let store = sb.tmp.path().join("credentials");
        let mut config = String::new();
        if with_helper {
            config.push_str(&format!(
                "[credential]
	helper = store --file={}
",
                fwd(&store)
            ));
        }
        std::fs::write(sb.user.join(".gitconfig"), config).unwrap();
        Self { sb, store }
    }

    fn stored(&self) -> String {
        std::fs::read_to_string(&self.store).unwrap_or_default()
    }

    fn seed_store(&self, world: &World, token: &str) {
        std::fs::write(
            &self.store,
            format!("http://asc:{token}@127.0.0.1:{}\n", world.hub.port),
        )
        .unwrap();
    }

    /// Runs `asc args` in `cwd`; `interactive` simulates a person at a terminal.
    fn run(
        &self,
        cwd: &Path,
        args: &[&str],
        interactive: bool,
        extra: &[(&str, &str)],
        stdin: &str,
    ) -> Output {
        let mut cmd = self.sb.asc(cwd);
        cmd.envs(IDENT).env("ASC_NO_DAEMON", "1").args(args);
        if interactive {
            cmd.env_remove("GIT_TERMINAL_PROMPT")
                .env("ASC_AUTH_INTERACTIVE", "1");
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn asc");
        let mut pipe = child.stdin.take().unwrap();
        let _ = pipe.write_all(stdin.as_bytes());
        drop(pipe);
        child.wait_with_output().unwrap()
    }
}

/// No token of this test appears in what a command printed.
fn never_leaks(out: &Output) {
    let (o, e) = (text(&out.stdout), text(&out.stderr));
    for t in ALL_TOKENS {
        assert!(
            !o.contains(t) && !e.contains(t),
            "a token was printed:\n{e}"
        );
    }
}

fn succeeded(out: &Output, what: &str) {
    assert!(out.status.success(), "{what} failed: {}", text(&out.stderr));
    never_leaks(out);
}

// ------------------------------------------------------------------------------ tests

#[test]
fn an_anonymous_clone_of_a_public_repository_needs_no_sign_in() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    for interactive in [false, true] {
        let dest = if interactive { "pub-tty" } else { "pub-pipe" };
        let out = m.run(
            &cwd,
            &["clone", &w.hub.url("acme/pub"), dest],
            interactive,
            &[],
            "",
        );
        succeeded(&out, "the public clone");
        assert!(cwd.join(dest).join("hello.txt").exists());
        assert!(!text(&out.stderr).contains("sign-in required"));
    }
    assert_eq!(w.hub.device_requests(), 0, "no sign-in was started");
    assert_eq!(m.stored(), "", "nothing was stored");
}

#[test]
fn a_private_clone_without_a_terminal_never_asks_and_says_what_to_do() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        false,
        &[],
        "",
    );
    assert!(!out.status.success());
    never_leaks(&out);
    let err = text(&out.stderr);
    assert!(err.contains("ASC_TOKEN"), "{err}");
    assert!(err.contains("credential helper"), "{err}");
    assert_eq!(w.hub.device_requests(), 0, "a script is never asked");
    assert!(!cwd.join("priv").join("hello.txt").exists());
}

#[test]
fn a_private_clone_signs_in_on_the_spot_retries_and_the_credential_is_reused() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &[],
        "",
    );
    succeeded(&out, "the private clone");
    let err = text(&out.stderr);
    assert!(err.contains("ABCD-EFGH"), "the code is shown: {err}");
    assert!(err.contains("login/device?user_code=ABCD-EFGH"), "{err}");
    assert!(cwd.join("priv").join("hello.txt").exists());
    let requests = w.hub.device_requests();
    assert!(requests >= 2, "the sign-in ran: {requests}");
    // The helper keeps the token for this host: the next clone, in a terminal or not, asks nothing.
    assert!(m.stored().contains(ALICE), "approved into the helper");
    for (interactive, dest) in [(true, "again-tty"), (false, "again-pipe")] {
        let out = m.run(
            &cwd,
            &["clone", &w.hub.url("acme/priv"), dest],
            interactive,
            &[],
            "",
        );
        succeeded(&out, "the second clone");
        assert!(cwd.join(dest).join("hello.txt").exists());
    }
    assert_eq!(w.hub.device_requests(), requests, "no second sign-in");
    assert!(
        !w.serve_log().contains(ALICE),
        "the server never logs a token"
    );
}

#[test]
fn a_credential_from_a_git_helper_is_used_without_asking() {
    let w = World::start();
    let m = Machine::new(&w, true);
    m.seed_store(&w, ALICE);
    let cwd = m.sb.dir("work");
    for (interactive, dest) in [(false, "a"), (true, "b")] {
        let out = m.run(
            &cwd,
            &["clone", &w.hub.url("acme/priv"), dest],
            interactive,
            &[],
            "",
        );
        succeeded(&out, "the clone with a stored credential");
        assert!(cwd.join(dest).join("hello.txt").exists());
    }
    assert_eq!(w.hub.device_requests(), 0);
}

#[test]
fn a_stale_stored_credential_is_dropped_and_the_sign_in_runs_once() {
    let w = World::start();
    let m = Machine::new(&w, true);
    m.seed_store(&w, STALE);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &[],
        "",
    );
    succeeded(&out, "the clone after a stale credential");
    assert!(cwd.join("priv").join("hello.txt").exists());
    let stored = m.stored();
    assert!(!stored.contains(STALE), "the helper forgot it: {stored}");
    assert!(stored.contains(ALICE), "and keeps the new one: {stored}");
}

#[test]
fn a_user_without_access_gets_one_clean_error_and_no_prompt_loop() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    // Bob signed in already (ASC_TOKEN): the server says no, and that is the end of it.
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &[("ASC_TOKEN", BOB)],
        "",
    );
    assert!(!out.status.success());
    never_leaks(&out);
    let err = text(&out.stderr);
    assert!(err.contains("no access"), "{err}");
    assert!(
        err.contains("not found"),
        "does not say whether it exists: {err}"
    );
    assert_eq!(
        w.hub.device_requests(),
        0,
        "no sign-in for a signed-in user"
    );

    // Bob signs in through the device flow: one sign-in, then the same clean error.
    w.hub.hands_out(BOB);
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv2"],
        true,
        &[],
        "",
    );
    assert!(!out.status.success());
    never_leaks(&out);
    assert!(
        text(&out.stderr).contains("no access"),
        "{}",
        text(&out.stderr)
    );
    let polls_and_start = w.hub.device_requests();
    assert!(polls_and_start <= 4, "one sign-in only: {polls_and_start}");
}

#[test]
fn a_repository_that_does_not_exist_looks_like_one_the_caller_may_not_see() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let (one, two) = (m.sb.dir("one"), m.sb.dir("two"));
    let missing = m.run(
        &one,
        &["clone", &w.hub.url("acme/nothing"), "d"],
        false,
        &[],
        "",
    );
    let private = m.run(
        &two,
        &["clone", &w.hub.url("acme/priv"), "d"],
        false,
        &[],
        "",
    );
    assert!(!missing.status.success() && !private.status.success());
    let strip = |o: &Output, name: &str| text(&o.stderr).replace(name, "<repo>");
    assert_eq!(
        strip(&missing, "acme/nothing"),
        strip(&private, "acme/priv"),
        "anonymous callers cannot tell the two apart"
    );
}

#[test]
fn a_push_to_a_public_repository_signs_in_when_the_server_asks() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/pub"), "pub"],
        true,
        &[],
        "",
    );
    succeeded(&out, "the public clone");
    let work = cwd.join("pub");
    assert_eq!(w.hub.device_requests(), 0);
    std::fs::write(work.join("more.txt"), "more\n").unwrap();
    succeeded(&m.run(&work, &["add", "more.txt"], false, &[], ""), "add");
    succeeded(
        &m.run(&work, &["commit", "-q", "-m", "more"], false, &[], ""),
        "commit",
    );

    // Without a terminal the push fails and says how to fix it.
    let out = m.run(&work, &["push", "origin", "main"], false, &[], "");
    assert!(!out.status.success());
    never_leaks(&out);
    assert!(
        text(&out.stderr).contains("ASC_TOKEN"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(w.hub.device_requests(), 0);

    // At a terminal it signs in, retries and succeeds.
    let out = m.run(&work, &["push", "origin", "main"], true, &[], "");
    succeeded(&out, "the push");
    assert!(
        text(&out.stderr).contains("ABCD-EFGH"),
        "{}",
        text(&out.stderr)
    );
    assert!(m.stored().contains(ALICE));
    // A fetch and ls-remote now run on the stored credential.
    let ls = m.run(&work, &["ls-remote", "origin"], false, &[], "");
    succeeded(&ls, "ls-remote");
    assert!(
        text(&ls.stdout).contains("refs/heads/main"),
        "{}",
        text(&ls.stdout)
    );
    succeeded(&m.run(&work, &["fetch", "origin"], false, &[], ""), "fetch");
}

#[test]
fn fetch_and_ls_remote_of_a_private_repository_ask_like_the_others() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    succeeded(&m.run(&cwd, &["init", "-q"], false, &[], ""), "init");
    let hub_url = w.hub.url("acme/priv");
    succeeded(
        &m.run(&cwd, &["remote", "add", "origin", &hub_url], false, &[], ""),
        "remote add",
    );
    for verb in ["ls-remote", "fetch"] {
        let out = m.run(&cwd, &[verb, "origin"], false, &[], "");
        assert!(!out.status.success(), "{verb} without a credential");
        never_leaks(&out);
        assert!(
            text(&out.stderr).contains("ASC_TOKEN"),
            "{}",
            text(&out.stderr)
        );
    }
    assert_eq!(w.hub.device_requests(), 0);
    // At a terminal a fetch signs in; the stored token then serves a fetch, a clone and an
    // ls-remote without one.
    let out = m.run(&cwd, &["fetch", "origin"], true, &[], "");
    succeeded(&out, "fetch at a terminal");
    assert!(
        text(&out.stderr).contains("ABCD-EFGH"),
        "{}",
        text(&out.stderr)
    );
    let requests = w.hub.device_requests();
    succeeded(
        &m.run(&cwd, &["fetch", "origin"], false, &[], ""),
        "fetch with the stored token",
    );
    succeeded(
        &m.run(&cwd, &["clone", &hub_url, "again"], false, &[], ""),
        "clone with the stored token",
    );
    let ls = m.run(&cwd.join("again"), &["ls-remote", "origin"], false, &[], "");
    succeeded(&ls, "ls-remote with the stored token");
    assert!(
        text(&ls.stdout).contains("refs/heads/main"),
        "{}",
        text(&ls.stdout)
    );
    assert_eq!(
        w.hub.device_requests(),
        requests,
        "one sign-in served them all"
    );
}

#[test]
fn without_a_browser_a_pasted_token_is_asked_for_and_never_echoed() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &[("ASC_NO_BROWSER", "1")],
        &format!("{ALICE}\n"),
    );
    succeeded(&out, "the clone with a pasted token");
    assert_eq!(w.hub.device_requests(), 0, "no device sign-in");
    assert!(cwd.join("priv").join("hello.txt").exists());
    assert!(m.stored().contains(ALICE));
}

#[test]
fn text_that_is_not_a_token_is_refused_at_the_prompt() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &[("ASC_NO_BROWSER", "1")],
        "not a token\n",
    );
    assert!(!out.status.success());
    never_leaks(&out);
    assert!(
        text(&out.stderr).contains("no usable token"),
        "{}",
        text(&out.stderr)
    );
}

// ------------------------------------------------------------------------------ the token store

/// With no credential helper asc keeps the token itself (the file store here, so the test never
/// touches a real keychain), and `asc auth logout` forgets it.
const FILE_STORE: [(&str, &str); 1] = [("ASC_CREDENTIAL_STORE", "file")];

#[test]
fn without_a_credential_helper_the_token_is_kept_and_reused_until_logout() {
    let w = World::start();
    let m = Machine::new(&w, false);
    let cwd = m.sb.dir("work");
    let file = m.sb.asc_home.join("credentials.json");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "one"],
        true,
        &FILE_STORE,
        "",
    );
    succeeded(&out, "the first private clone");
    assert!(
        text(&out.stderr).contains("kept in"),
        "{}",
        text(&out.stderr)
    );
    let kept = std::fs::read_to_string(&file).unwrap();
    assert!(kept.contains(ALICE), "the token is in the private file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let requests = w.hub.device_requests();
    assert!(requests >= 2);

    // The next runs, at a terminal or in a script, ask nothing.
    for (interactive, dest) in [(true, "two"), (false, "three")] {
        let out = m.run(
            &cwd,
            &["clone", &w.hub.url("acme/priv"), dest],
            interactive,
            &FILE_STORE,
            "",
        );
        succeeded(&out, "a clone on the kept token");
        assert!(cwd.join(dest).join("hello.txt").exists());
    }
    assert_eq!(w.hub.device_requests(), requests, "no second sign-in");

    // `asc auth status` names the source, never the token.
    let status = m.run(
        &cwd,
        &["auth", "status", &w.hub.url("acme/priv")],
        false,
        &FILE_STORE,
        "",
    );
    succeeded(&status, "auth status");
    assert!(
        text(&status.stdout).contains("credential source: asc credential store"),
        "{}",
        text(&status.stdout)
    );

    // Logout forgets it: a script is refused again.
    let out = m.run(
        &cwd,
        &[
            "auth",
            "logout",
            &format!("http://127.0.0.1:{}", w.hub.port),
        ],
        false,
        &FILE_STORE,
        "",
    );
    succeeded(&out, "logout");
    assert!(
        !std::fs::read_to_string(&file)
            .unwrap_or_default()
            .contains(ALICE)
    );
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "four"],
        false,
        &FILE_STORE,
        "",
    );
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("ASC_TOKEN"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_kept_token_the_server_refuses_is_forgotten_and_the_sign_in_runs_again() {
    let w = World::start();
    let m = Machine::new(&w, false);
    let cwd = m.sb.dir("work");
    let file = m.sb.asc_home.join("credentials.json");
    std::fs::create_dir_all(&m.sb.asc_home).unwrap();
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"tokens\":{{\"http://127.0.0.1:{}\":\"{STALE}\"}}}}",
            w.hub.port
        ),
    )
    .unwrap();
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &FILE_STORE,
        "",
    );
    succeeded(&out, "the clone after a stale kept token");
    let kept = std::fs::read_to_string(&file).unwrap();
    assert!(!kept.contains(STALE) && kept.contains(ALICE), "{kept}");
    assert!(w.hub.device_requests() >= 2);
}

#[test]
fn a_configured_helper_is_left_alone_by_the_token_store() {
    let w = World::start();
    let m = Machine::new(&w, true);
    let cwd = m.sb.dir("work");
    let out = m.run(
        &cwd,
        &["clone", &w.hub.url("acme/priv"), "priv"],
        true,
        &FILE_STORE,
        "",
    );
    succeeded(&out, "the clone with a helper");
    assert!(m.stored().contains(ALICE), "git's helper keeps it");
    assert!(
        !m.sb.asc_home.join("credentials.json").exists(),
        "asc keeps nothing of its own"
    );
}
