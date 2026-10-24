//! U83: the git client compatibility suite (SPEC §25.10, §17.1; PLAN U83).
//!
//! Real git >= 2.40 (through `RealGit`) runs against the real `asc serve` binary with the token
//! store, and against a plain git server (`git http-backend` behind an axum fixture) for the
//! same commands, so "git-identical" is measured, not assumed. `clients.rs` holds the client
//! cases, the credential paths and the whole-stack security and limit cases, `migrate.rs` the
//! mirror migration, `branchless.rs` the cases of SPEC §25.6.
//!
//! This file holds the harness and the build without the layer: with `git-compat` off, a server
//! from `asc_serve::serve` answers the disabled stub (`401` challenge, then `403`) for the three
//! git paths. `scripts/ci/no-git-compat.sh` runs that case with `--no-default-features
//! --features serve`.
#![allow(
    dead_code,
    clippy::new_without_default,
    clippy::unwrap_used,
    clippy::expect_used
)]

#[cfg(all(feature = "serve", feature = "git-compat"))]
mod branchless;
#[cfg(all(feature = "serve", feature = "git-compat"))]
mod clients;
#[cfg(all(feature = "serve", feature = "git-compat"))]
mod migrate;

#[cfg(feature = "serve")]
pub mod harness {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use asc_git::realgit::RealGit;

    /// A free loopback port (bound and released; another process could race for it, which the
    /// retry in `Stack::start` covers).
    pub fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    pub fn fwd(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    /// One answer of [`http`].
    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Reply {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    /// A bare HTTP/1.1 request over a fresh connection (`Connection: close`), so the harness
    /// sees exactly the bytes on the wire and needs no HTTP client.
    pub fn http(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(raw.len());
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let mut body = raw.get(split + 4..).unwrap_or_default().to_vec();
        if headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"))
        {
            body = dechunk(&body);
        }
        Reply {
            status,
            headers,
            body,
        }
    }

    fn dechunk(mut b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(eol) = b.windows(2).position(|w| w == b"\r\n") {
            let size =
                usize::from_str_radix(std::str::from_utf8(&b[..eol]).unwrap_or("0").trim(), 16)
                    .unwrap_or(0);
            if size == 0 {
                break;
            }
            let start = eol + 2;
            out.extend_from_slice(&b[start..start + size]);
            b = &b[start + size + 2..];
        }
        out
    }

    pub fn basic(user: &str, secret: &str) -> String {
        format!("Basic {}", b64(format!("{user}:{secret}").as_bytes()))
    }

    pub fn b64(input: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in input.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// The result of one git run.
    pub struct Run {
        pub ok: bool,
        pub stdout: String,
        pub stderr: String,
    }

    /// Real git with a private home, no system config, no prompts and no inherited `GIT_*`
    /// or credential variables.
    pub struct Git {
        real: RealGit,
        pub home: tempfile::TempDir,
    }

    impl Git {
        pub fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(
                home.path().join("gitconfig"),
                "[user]\n\tname = T\n\temail = t@x\n[init]\n\tdefaultBranch = master\n\
                 [core]\n\tautocrlf = false\n[advice]\n\tdetachedHead = false\n\
                 [protocol \"file\"]\n\tallow = always\n",
            )
            .unwrap();
            Self {
                real: RealGit::resolve(None, None).expect("real git >= 2.40 is required"),
                home,
            }
        }

        /// Appends `text` (git config syntax) to the global config.
        pub fn config(&self, text: &str) {
            let p = self.home.path().join("gitconfig");
            let mut cur = std::fs::read_to_string(&p).unwrap();
            cur.push_str(text);
            std::fs::write(&p, cur).unwrap();
        }

        pub fn work(&self, name: &str) -> PathBuf {
            let p = self.home.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        }

        pub fn run_env(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
            let mut cmd = self.real.command().unwrap();
            for (k, _) in std::env::vars_os() {
                let name = k.to_string_lossy().to_ascii_uppercase();
                if name.starts_with("GIT_") || name.starts_with("ASC_") || name.ends_with("ASKPASS")
                {
                    cmd.env_remove(&k);
                }
            }
            cmd.args(args)
                .current_dir(cwd)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.home.path().join("gitconfig"))
                .env("HOME", self.home.path())
                .env("USERPROFILE", self.home.path())
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            Run {
                ok: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
        }

        pub fn run(&self, cwd: &Path, args: &[&str]) -> Run {
            self.run_env(cwd, args, &[])
        }

        /// Runs git, which must succeed; returns trimmed stdout.
        pub fn ok(&self, cwd: &Path, args: &[&str]) -> String {
            let r = self.run(cwd, args);
            assert!(r.ok, "git {args:?} failed: {}", mask(&r.stderr));
            r.stdout.trim().to_owned()
        }

        /// Runs git with `input` on stdin, which must succeed.
        pub fn ok_stdin(&self, cwd: &Path, args: &[&str], input: &[u8]) -> String {
            let mut cmd = self.real.command().unwrap();
            cmd.args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.home.path().join("gitconfig"))
                .env("HOME", self.home.path())
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let input = input.to_vec();
            let feeder = std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
            let out = child.wait_with_output().unwrap();
            let _ = feeder.join();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        /// A repository `name` with `n` commits on master (file `f<i>` with the text `i`).
        pub fn repo_with_commits(&self, name: &str, n: usize) -> PathBuf {
            let w = self.work(name);
            self.ok(&w, &["init", "-q"]);
            for i in 0..n {
                std::fs::write(w.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
                self.ok(&w, &["add", "."]);
                self.ok(&w, &["commit", "-q", "-m", &format!("c{i}")]);
            }
            w
        }

        pub fn head(&self, cwd: &Path) -> String {
            self.ok(cwd, &["rev-parse", "HEAD"])
        }
    }

    /// Hides anything that looks like one of our tokens in a message.
    pub fn mask(text: &str) -> String {
        let mut out = String::new();
        for word in text.split_inclusive(|c: char| c.is_whitespace() || c == '/' || c == '@') {
            let w = word.trim_end_matches(|c: char| c.is_whitespace() || c == '/' || c == '@');
            if w.len() >= 24
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                out.push_str("<masked>");
                out.push_str(&word[w.len()..]);
            } else {
                out.push_str(word);
            }
        }
        out
    }

    /// A token minted by `asc serve token create`.
    #[derive(Clone)]
    pub struct Token {
        pub id: String,
        pub secret: String,
    }

    /// A running `asc serve` (the real binary, the token store, a SQLite database and an in-memory
    /// object store). Dropping it kills the process.
    pub struct Stack {
        pub dir: tempfile::TempDir,
        pub port: u16,
        pub admin_port: u16,
        pub admin: Token,
        child: Option<Child>,
        cfg: PathBuf,
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

    impl Stack {
        /// `git_section` is the text of `[git]` (without the header), `limits` the lines of
        /// `[limits.git]`, `extra` further tables.
        pub fn start_with(git_section: &str, limits: &str, extra: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let port = free_port();
            let admin_port = free_port();
            let cfg = dir.path().join("asc-serve.toml");
            let gate = if limits.contains("min_free_disk") {
                ""
            } else {
                "min_free_disk = \"1KiB\"\n"
            };
            std::fs::write(
                &cfg,
                format!(
                    "listen = \"127.0.0.1:{port}\"\npublic_url = \"http://127.0.0.1:{port}\"\n\
                     store = \"memory:///\"\n\
                     db =\"sqlite:{db}?mode=rwc\"\n\
                     cache = {{ disk_dir = {cache:?}, disk = \"1GiB\" }}\n\
                     [auth]\nprovider = \"token_store\"\ntoken_key_file = {key:?}\n\
                     [admin]\nlisten = \"127.0.0.1:{admin_port}\"\n\
                     [git]\n{git_section}\n[limits.git]\n{gate}{limits}\n{extra}\n",
                    db = fwd(&dir.path().join("asc.db")),
                    cache = fwd(&dir.path().join("cache")),
                    key = fwd(&dir.path().join("token.key")),
                ),
            )
            .unwrap();
            std::fs::create_dir_all(dir.path().join("asc-home")).unwrap();
            let mut stack = Self {
                dir,
                port,
                admin_port,
                admin: Token {
                    id: String::new(),
                    secret: String::new(),
                },
                child: None,
                cfg,
            };
            let m = stack.serve(&["migrate"]);
            assert!(m.status.success(), "migrate: {}", lossy(&m.stderr));
            stack.admin = stack.token(&["--user", "root", "--repo", "acme/*", "--admin"]);
            let log = std::fs::File::create(stack.dir.path().join("serve.log")).unwrap();
            let mut cmd = Command::new(asc_exe());
            clean(&mut cmd, stack.dir.path());
            cmd.arg("serve")
                .arg("--config")
                .arg(&stack.cfg)
                .current_dir(stack.dir.path())
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log));
            stack.child = Some(cmd.spawn().expect("spawn asc serve"));
            stack.wait_ready();
            stack
        }

        pub fn start() -> Self {
            Self::start_with("enabled = true\n", "", "")
        }

        pub fn addr(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], self.port))
        }

        pub fn log(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("serve.log")).unwrap_or_default()
        }

        fn wait_ready(&mut self) {
            let end = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(c) = self.child.as_mut()
                    && let Ok(Some(status)) = c.try_wait()
                {
                    panic!("asc serve exited early ({status}): {}", self.log());
                }
                if TcpStream::connect_timeout(&self.addr(), Duration::from_millis(200)).is_ok()
                    && http(self.addr(), "GET", "/healthz", &[], b"").status == 200
                {
                    return;
                }
                assert!(
                    Instant::now() < end,
                    "asc serve never listened: {}",
                    self.log()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        fn serve(&self, args: &[&str]) -> std::process::Output {
            let mut cmd = Command::new(asc_exe());
            clean(&mut cmd, self.dir.path());
            cmd.arg("serve")
                .arg("--config")
                .arg(&self.cfg)
                .args(args)
                .current_dir(self.dir.path());
            cmd.output().expect("run asc serve")
        }

        /// `asc serve token create <flags>`.
        pub fn token(&self, flags: &[&str]) -> Token {
            let mut args = vec!["token", "create"];
            args.extend_from_slice(flags);
            let out = self.serve(&args);
            assert!(out.status.success(), "token create: {}", lossy(&out.stderr));
            let secret = lossy(&out.stdout).trim().to_owned();
            let id = lossy(&out.stderr)
                .lines()
                .find_map(|l| l.strip_prefix("id:"))
                .map(|s| s.trim().to_owned())
                .expect("the token id");
            Token { id, secret }
        }

        pub fn revoke(&self, t: &Token) {
            let out = self.serve(&["token", "revoke", &t.id]);
            assert!(out.status.success(), "revoke: {}", lossy(&out.stderr));
        }

        /// `RepoAdmin init` through `asc api`, as the admin token.
        pub fn create_repo(&self, name: &str, init: &str) {
            let url = format!("http://127.0.0.1:{}/{name}", self.port);
            let req = format!(r#"{{"repo":{{"name":"{name}"}},"init":{{{init}}}}}"#);
            let mut cmd = Command::new(asc_exe());
            clean(&mut cmd, self.dir.path());
            let out = cmd
                .args(["api", "RepoAdmin", "--remote", &url, "--json", &req])
                .env("ASC_TOKEN", &self.admin.secret)
                .current_dir(self.dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "RepoAdmin init {name}: {}",
                mask(&lossy(&out.stderr))
            );
        }

        pub fn url(&self, repo: &str) -> String {
            format!("http://127.0.0.1:{}/{repo}.git", self.port)
        }

        pub fn url_with(&self, repo: &str, token: &Token) -> String {
            format!(
                "http://x-access-token:{}@127.0.0.1:{}/{repo}.git",
                token.secret, self.port
            )
        }

        pub fn host(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        /// Whether the server is still running.
        pub fn alive(&mut self) -> bool {
            self.child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
        }
    }

    impl Drop for Stack {
        fn drop(&mut self) {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    pub fn lossy(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    /// Every ref of a repository as `name sha` lines, sorted, for exact comparisons.
    pub fn refs_of(git: &Git, repo: &Path) -> Vec<String> {
        let mut v: Vec<String> = git
            .ok(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .map(str::to_owned)
            .collect();
        v.sort();
        v
    }
}

/// The plain git server the asc output is compared with: `git http-backend` behind axum.
#[cfg(all(feature = "serve", feature = "git-compat"))]
pub mod plain {
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderName, HeaderValue, Request, StatusCode};
    use axum::response::Response;

    use crate::harness::Git;

    #[derive(Clone)]
    struct Cgi {
        root: PathBuf,
        git: PathBuf,
        home: PathBuf,
    }

    pub struct Plain {
        pub addr: SocketAddr,
        pub root: PathBuf,
    }

    impl Plain {
        /// Serves bare repositories under `root` (created by the caller, `<name>.git`) on its
        /// own runtime thread, so tests can run git synchronously.
        pub fn start(git: &Git, root: &Path) -> Self {
            let real = asc_git::realgit::RealGit::resolve(None, None).unwrap();
            let state = Cgi {
                root: root.to_owned(),
                git: real.path().to_owned(),
                home: git.home.path().to_owned(),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let app = Router::new().fallback(handle).with_state(state);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    let _ = axum::serve(listener, app).await;
                });
            });
            Self {
                addr: rx.recv().unwrap(),
                root: root.to_owned(),
            }
        }

        /// A bare repository `name` that accepts pushes.
        pub fn create(&self, git: &Git, name: &str) -> PathBuf {
            let p = self.root.join(format!("{name}.git"));
            std::fs::create_dir_all(&p).unwrap();
            git.ok(&p, &["init", "-q", "--bare"]);
            git.ok(&p, &["config", "http.receivepack", "true"]);
            git.ok(&p, &["config", "uploadpack.allowFilter", "true"]);
            git.ok(&p, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
            p
        }

        pub fn url(&self, name: &str) -> String {
            format!("http://{}/{name}.git", self.addr)
        }
    }

    async fn handle(State(cgi): State<Cgi>, req: Request<Body>) -> Response {
        let (parts, body) = req.into_parts();
        let body: Bytes = axum::body::to_bytes(body, 512 * 1024 * 1024)
            .await
            .unwrap_or_default();
        let header = |n: &str| {
            parts
                .headers
                .get(n)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        let mut cmd = tokio::process::Command::new(&cgi.git);
        cmd.arg("http-backend")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(
                "SystemRoot",
                std::env::var_os("SystemRoot").unwrap_or_default(),
            )
            .env("HOME", &cgi.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_PROJECT_ROOT", &cgi.root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("REQUEST_METHOD", parts.method.as_str())
            .env("PATH_INFO", parts.uri.path())
            .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
            .env("CONTENT_TYPE", header("content-type"))
            .env("CONTENT_LENGTH", body.len().to_string())
            .env("GIT_PROTOCOL", header("git-protocol"))
            .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
            .env("REMOTE_USER", "plain")
            .env("REMOTE_ADDR", "127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&body).await;
        });
        let out = child.wait_with_output().await.unwrap();
        let _ = writer.await;
        let raw = out.stdout;
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
        let Some((at, skip)) = split else {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap();
        };
        let mut status = StatusCode::OK;
        let mut resp = Response::builder();
        for line in String::from_utf8_lossy(&raw[..at]).lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if k.eq_ignore_ascii_case("status") {
                if let Some(code) = v.trim().split(' ').next().and_then(|c| c.parse().ok()) {
                    status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
                }
            } else if let (Ok(k), Ok(v)) = (
                HeaderName::from_bytes(k.trim().as_bytes()),
                HeaderValue::from_str(v.trim()),
            ) {
                resp = resp.header(k, v);
            }
        }
        resp.status(status)
            .body(Body::from(raw[at + skip..].to_vec()))
            .unwrap()
    }
}

// ============================================================================ the build without
// the layer: `asc serve` is the real library entry, `git-compat` is off, `[git] enabled = true`
// is configured, and the three git paths answer the stub.

#[cfg(all(feature = "serve", not(feature = "git-compat")))]
mod stub {
    use std::sync::Arc;

    use asc_rpc::auth::{Access, StaticTokens};

    use crate::harness::{basic, free_port, fwd, http};

    const READER: &str = "reader-secret-token-0123456789";

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_build_without_git_compat_serves_the_stub_on_all_three_paths() {
        let dir = tempfile::tempdir().unwrap();
        let (port, admin) = (free_port(), free_port());
        let cfg = asc_serve::ServeConfig::parse(&format!(
            "listen = \"127.0.0.1:{port}\"\npublic_url = \"https://asc.example.org\"\n\
             store = \"memory:///\"\n\
             db =\"sqlite:{}?mode=rwc\"\n[admin]\nlisten = \"127.0.0.1:{admin}\"\n\
             [git]\nenabled = true\n",
            fwd(&dir.path().join("asc.db"))
        ))
        .unwrap();
        asc_serve::run_migrate(&cfg).await.unwrap();
        let authz = Arc::new(StaticTokens::new().with_token(READER, "reader", Access::Read));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(asc_serve::serve(cfg, authz, async {
            let _ = stopped.await;
        }));
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if server.is_finished() {
                panic!("the server stopped: {:?}", server.await);
            }
            if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200))
                .is_ok()
                && http(addr, "GET", "/healthz", &[], b"").status == 200
            {
                break;
            }
            assert!(std::time::Instant::now() < end, "the server never listened");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let paths = [
            ("GET", "/acme/web.git/info/refs?service=git-upload-pack"),
            ("POST", "/acme/web.git/git-upload-pack"),
            ("POST", "/acme/web.git/git-receive-pack"),
        ];
        let auth = basic("x-access-token", READER);
        for (method, path) in paths {
            // Anonymous: the Basic challenge, as for a git host.
            let anon = tokio::task::block_in_place(|| http(addr, method, path, &[], b"0000"));
            assert_eq!(anon.status, 401, "{path}: anonymous");
            assert!(
                anon.header("www-authenticate")
                    .is_some_and(|v| v.starts_with("Basic")),
                "{path}: the Basic challenge"
            );
            // Authenticated: the stub, never git.
            let known = tokio::task::block_in_place(|| {
                http(addr, method, path, &[("Authorization", &auth)], b"0000")
            });
            assert_eq!(known.status, 403, "{path}: authenticated");
            assert_eq!(known.header("x-asc-git"), Some("disabled"), "{path}");
            assert!(
                known.text().contains("git protocol disabled"),
                "{path}: {}",
                known.text()
            );
            assert!(!known.text().contains(READER), "the body repeats no secret");
        }
        let _ = stop.send(());
        let _ = server.await;
    }
}

#[cfg(all(feature = "serve", feature = "git-compat"))]
mod smoke {
    use crate::harness::*;

    #[test]
    fn clone_push_and_clone_again_through_the_real_server() {
        let stack = Stack::start();
        stack.create_repo("acme/web", "");
        let write = stack.token(&["--user", "alice", "--repo", "acme/web", "--write"]);
        let git = Git::new();
        let a = git.repo_with_commits("a", 2);
        let url = stack.url_with("acme/web", &write);
        git.ok(&a, &["remote", "add", "origin", &url]);
        let pushed = git.run(&a, &["push", "origin", "master"]);
        assert!(pushed.ok, "push: {}", mask(&pushed.stderr));
        let b = git.work("b");
        let cloned = git.run(&b, &["clone", &url, "b2"]);
        assert!(cloned.ok, "clone: {}", mask(&cloned.stderr));
        assert_eq!(git.head(&b.join("b2")), git.head(&a));
    }
}
