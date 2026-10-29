//! U88: the SSH compatibility suite (SPEC §25.11, §15.6.1, §17.1; PLAN U88).
//!
//! Real git >= 2.40 (through `RealGit`) and a real OpenSSH `ssh`, `ssh-keygen`, `ssh-add` and
//! `ssh-agent` run against the real `asc serve` binary (token store, SSH listener on an
//! ephemeral loopback port). `GIT_SSH_COMMAND` sets only `-p`, `-i`, `-o IdentitiesOnly=yes` and
//! a `UserKnownHostsFile` pre-seeded with the server's host key, so host-key checking stays on.
//!
//! - this file: the harness and the git cases (clone, fetch, pull, push, shallow, partial,
//!   protocol versions) and the build without `git-ssh`;
//! - `openssh.rs`: key types, the agent, `~/.ssh/config`, URL forms, the refusals, host keys and
//!   the no-weakening checks;
//! - `migrate.rs`: the mirror migration, and the in-process rig for the forge, the branchless
//!   cases of SPEC §25.6, audit and admission.
//!
//! The tools are required in CI (`CI` or `ASC_REQUIRE_SSH_TOOLS` set): a missing tool fails the
//! suite loudly. Locally a missing tool skips a test with a message.
#![allow(
    dead_code,
    clippy::new_without_default,
    clippy::unwrap_used,
    clippy::expect_used
)]
#![cfg(all(feature = "serve", feature = "git-compat"))]

#[cfg(feature = "git-ssh")]
mod migrate;
#[cfg(feature = "git-ssh")]
mod openssh;

#[path = "../support/mod.rs"]
mod support;

pub mod h {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use asc_git::realgit::RealGit;

    /// Keeps a child `ssh` (and anything it starts) from opening `/dev/tty`. Run from a terminal,
    /// an unknown host key or a passphrase would otherwise be asked for there and the test would
    /// wait for a person (or stop on SIGTTIN when the group is in the background). With
    /// `SSH_ASKPASS_REQUIRE=force` every prompt goes to a program that answers nothing, so ssh
    /// fails the way it does with no terminal. OpenSSH 8.4 and later; older ones ignore both
    /// variables and keep the terminal.
    pub fn no_tty(cmd: &mut Command) -> &mut Command {
        #[cfg(unix)]
        {
            use std::sync::OnceLock;
            static ASKPASS: OnceLock<PathBuf> = OnceLock::new();
            let path = ASKPASS.get_or_init(|| {
                use std::os::unix::fs::PermissionsExt;
                let path =
                    std::env::temp_dir().join(format!("asc-no-askpass-{}.sh", std::process::id()));
                std::fs::write(
                    &path,
                    "#!/bin/sh
exit 1
",
                )
                .unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
                path
            });
            cmd.env("SSH_ASKPASS", path)
                .env("SSH_ASKPASS_REQUIRE", "force");
        }
        cmd
    }

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

    pub fn lossy(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    /// Hides anything that looks like one of our secrets in a message.
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

    // ------------------------------------------------------------------------------ the tools

    /// The OpenSSH client programs.
    #[derive(Clone)]
    pub struct Tools {
        pub ssh: PathBuf,
        pub keygen: PathBuf,
        pub agent: PathBuf,
        pub add: PathBuf,
    }

    fn which(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            for file in [name.to_owned(), format!("{name}.exe")] {
                let p = dir.join(&file);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        None
    }

    /// Whether a missing tool is an error (CI) rather than a skip.
    pub fn strict() -> bool {
        std::env::var_os("CI").is_some() || std::env::var_os("ASC_REQUIRE_SSH_TOOLS").is_some()
    }

    /// The tools, or `None` (after a message) when they are missing and the run is local.
    ///
    /// # Panics
    /// When a tool is missing and the run is strict.
    pub fn tools() -> Option<Tools> {
        let found = (|| {
            Some(Tools {
                ssh: which("ssh")?,
                keygen: which("ssh-keygen")?,
                agent: which("ssh-agent")?,
                add: which("ssh-add")?,
            })
        })();
        if found.is_none() {
            assert!(
                !strict(),
                "OpenSSH ssh, ssh-keygen, ssh-agent and ssh-add must be on PATH in CI"
            );
            eprintln!("skipped: OpenSSH ssh, ssh-keygen, ssh-agent and ssh-add are not on PATH");
        }
        found
    }

    /// Returns from the test (a skip) when the OpenSSH tools are missing.
    #[macro_export]
    macro_rules! need_tools {
        () => {
            match $crate::h::tools() {
                Some(t) => t,
                None => return,
            }
        };
    }

    /// An SSH key pair made by `ssh-keygen`.
    #[derive(Clone)]
    pub struct Key {
        pub path: PathBuf,
        pub public: PathBuf,
    }

    impl Key {
        /// The `authorized_keys` line.
        pub fn line(&self) -> String {
            std::fs::read_to_string(&self.public)
                .unwrap()
                .trim()
                .to_owned()
        }

        /// The base64 public blob.
        pub fn blob(&self) -> String {
            self.line().split_whitespace().nth(1).unwrap().to_owned()
        }

        /// The base64 private key body, for canary searches.
        pub fn private_body(&self) -> String {
            std::fs::read_to_string(&self.path)
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with("-----"))
                .collect()
        }
    }

    impl Tools {
        /// `kind` is `ed25519`, `ecdsa` or `rsa`.
        pub fn key(&self, dir: &Path, name: &str, kind: &str) -> Key {
            let path = dir.join(name);
            let mut cmd = Command::new(&self.keygen);
            cmd.args(["-q", "-t", kind]);
            match kind {
                "rsa" => cmd.args(["-b", "3072"]),
                "ecdsa" => cmd.args(["-b", "256"]),
                _ => &mut cmd,
            };
            let out = cmd
                .args(["-N", "", "-C", name, "-f"])
                .arg(&path)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "ssh-keygen: {}", lossy(&out.stderr));
            Key {
                public: PathBuf::from(format!("{}.pub", path.display())),
                path,
            }
        }

        /// A `GIT_SSH_COMMAND` with only `-p`, `-i`, `IdentitiesOnly` and the known-hosts file.
        pub fn command(
            &self,
            port: Option<u16>,
            key: Option<&Key>,
            known_hosts: &Path,
            extra: &[&str],
        ) -> String {
            let mut s = format!("\"{}\"", fwd(&self.ssh));
            if let Some(p) = port {
                s.push_str(&format!(" -p {p}"));
            }
            if let Some(k) = key {
                s.push_str(&format!(" -i \"{}\" -o IdentitiesOnly=yes", fwd(&k.path)));
            }
            s.push_str(&format!(" -o \"UserKnownHostsFile={}\"", fwd(known_hosts)));
            for e in extra {
                s.push(' ');
                s.push_str(e);
            }
            s
        }

        /// Runs `ssh` itself with the options of [`Tools::command`] and `args`.
        pub fn run_ssh(
            &self,
            home: &Path,
            port: u16,
            key: Option<&Key>,
            known_hosts: &Path,
            args: &[&str],
        ) -> Run {
            let mut cmd = Command::new(&self.ssh);
            cmd.args(["-p", &port.to_string()]);
            if let Some(k) = key {
                cmd.arg("-i")
                    .arg(&k.path)
                    .args(["-o", "IdentitiesOnly=yes"]);
            }
            cmd.arg("-o")
                .arg(format!("UserKnownHostsFile={}", fwd(known_hosts)))
                .args(["-o", "ConnectTimeout=20"])
                .args(args)
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env_remove("SSH_AUTH_SOCK")
                .stdin(Stdio::null());
            no_tty(&mut cmd);
            let out = cmd.output().unwrap();
            Run {
                ok: out.status.success(),
                code: out.status.code(),
                stdout: lossy(&out.stdout),
                stderr: lossy(&out.stderr),
            }
        }
    }

    /// A running `ssh-agent`, killed on drop.
    pub struct Agent {
        pub sock: String,
        pid: String,
        tools: Tools,
    }

    impl Agent {
        /// Starts an agent; `None` when this platform's agent cannot be driven from here.
        pub fn start(tools: &Tools) -> Option<Self> {
            let out = Command::new(&tools.agent)
                .arg("-s")
                .stdin(Stdio::null())
                .output()
                .ok()?;
            let text = lossy(&out.stdout);
            let var = |name: &str| {
                text.split(';')
                    .find_map(|p| p.trim().strip_prefix(&format!("{name}=")))
                    .map(str::to_owned)
            };
            Some(Self {
                sock: var("SSH_AUTH_SOCK")?,
                pid: var("SSH_AGENT_PID")?,
                tools: tools.clone(),
            })
        }

        pub fn add(&self, key: &Key) -> bool {
            Command::new(&self.tools.add)
                .arg(&key.path)
                .env("SSH_AUTH_SOCK", &self.sock)
                .env("SSH_AGENT_PID", &self.pid)
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success())
        }
    }

    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = Command::new(&self.tools.agent)
                .arg("-k")
                .env("SSH_AUTH_SOCK", &self.sock)
                .env("SSH_AGENT_PID", &self.pid)
                .output();
        }
    }

    // ------------------------------------------------------------------------------ git

    /// The result of one run.
    pub struct Run {
        pub ok: bool,
        pub code: Option<i32>,
        pub stdout: String,
        pub stderr: String,
    }

    /// Real git with a private home, no system config, no prompts and no inherited `GIT_*`.
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
                 [core]\n\tautocrlf = false\n[advice]\n\tdetachedHead = false\n",
            )
            .unwrap();
            Self {
                real: RealGit::resolve(None, None).expect("real git >= 2.40 is required"),
                home,
            }
        }

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
                if name.starts_with("GIT_")
                    || name.starts_with("ASC_")
                    || name.ends_with("ASKPASS")
                    || name == "SSH_AUTH_SOCK"
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
            no_tty(&mut cmd);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            Run {
                ok: out.status.success(),
                code: out.status.code(),
                stdout: lossy(&out.stdout),
                stderr: lossy(&out.stderr),
            }
        }

        /// Runs git with `GIT_SSH_COMMAND` set to `ssh`.
        pub fn ssh(&self, cwd: &Path, ssh: &str, args: &[&str]) -> Run {
            self.run_env(cwd, args, &[("GIT_SSH_COMMAND", ssh)])
        }

        /// As [`Git::ssh`], which must succeed; returns trimmed stdout.
        pub fn ssh_ok(&self, cwd: &Path, ssh: &str, args: &[&str]) -> String {
            let r = self.ssh(cwd, ssh, args);
            assert!(r.ok, "git {args:?} failed: {}", mask(&r.stderr));
            r.stdout.trim().to_owned()
        }

        /// Local git, which must succeed.
        pub fn ok(&self, cwd: &Path, args: &[&str]) -> String {
            let r = self.run_env(cwd, args, &[]);
            assert!(r.ok, "git {args:?} failed: {}", mask(&r.stderr));
            r.stdout.trim().to_owned()
        }

        pub fn run(&self, cwd: &Path, args: &[&str]) -> Run {
            self.run_env(cwd, args, &[])
        }

        pub fn repo_with_commits(&self, name: &str, n: usize) -> PathBuf {
            let w = self.work(name);
            self.ok(&w, &["init", "-q"]);
            for i in 0..n {
                self.commit(
                    &w,
                    &format!("f{i}.txt"),
                    &format!("{i}\n"),
                    &format!("c{i}"),
                );
            }
            w
        }

        pub fn commit(&self, w: &Path, file: &str, body: &str, msg: &str) {
            std::fs::write(w.join(file), body).unwrap();
            self.ok(w, &["add", "."]);
            self.ok(w, &["commit", "-q", "-m", msg]);
        }

        pub fn head(&self, cwd: &Path) -> String {
            self.ok(cwd, &["rev-parse", "HEAD"])
        }

        /// Every ref as `name sha` lines, sorted.
        pub fn refs(&self, repo: &Path) -> Vec<String> {
            let mut v: Vec<String> = self
                .ok(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
                .lines()
                .map(str::to_owned)
                .collect();
            v.sort();
            v
        }
    }

    // ------------------------------------------------------------------------------ the server

    pub fn asc_exe() -> &'static str {
        env!("CARGO_BIN_EXE_asc")
    }

    fn clean(cmd: &mut Command, home: &Path) {
        for (k, _) in std::env::vars_os() {
            let name = k.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("GIT_") || name.starts_with("ASC_") || name == "SSH_AUTH_SOCK" {
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

    #[derive(Clone)]
    pub struct Token {
        pub id: String,
        pub secret: String,
    }

    /// A running `asc serve` (the real binary, the token store, SQLite, an in-memory object
    /// store) with the SSH listener on an ephemeral port. Dropping it kills the process.
    pub struct Stack {
        pub dir: tempfile::TempDir,
        pub port: u16,
        pub ssh_port: u16,
        pub admin: Token,
        pub known_hosts: PathBuf,
        child: Option<Child>,
        cfg: PathBuf,
    }

    impl Stack {
        /// `ssh` is extra lines of `[ssh]`, `extra` further tables.
        pub fn start_with(ssh: &str, extra: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let port = free_port();
            let admin_port = free_port();
            let ssh_port = free_port();
            let cfg = dir.path().join("asc-serve.toml");
            std::fs::write(
                &cfg,
                format!(
                    "listen = \"127.0.0.1:{port}\"\npublic_url = \"http://127.0.0.1:{port}\"\n\
                     store = \"memory:///\"\n\
                     db = \"sqlite:{db}?mode=rwc\"\n\
                     cache = {{ disk_dir = {cache:?}, disk = \"1GiB\" }}\n\
                     [auth]\nprovider = \"token_store\"\ntoken_key_file = {key:?}\n\
                     [admin]\nlisten = \"127.0.0.1:{admin_port}\"\n\
                     [git]\nenabled = true\n\
                     [limits.git]\nmin_free_disk = \"1KiB\"\n\
                     [ssh]\nenabled = true\nlisten = \"127.0.0.1:{ssh_port}\"\n\
                     host_key_files = [{host:?}]\n{ssh}\n\
                     [limits.ssh]\nreject_delay_ms = 0\n{extra}\n",
                    db = fwd(&dir.path().join("asc.db")),
                    cache = fwd(&dir.path().join("cache")),
                    key = fwd(&dir.path().join("token.key")),
                    host = fwd(&dir.path().join("ssh_host_ed25519_key")),
                ),
            )
            .unwrap();
            std::fs::create_dir_all(dir.path().join("asc-home")).unwrap();
            let known_hosts = dir.path().join("known_hosts");
            let mut stack = Self {
                dir,
                port,
                ssh_port,
                admin: Token {
                    id: String::new(),
                    secret: String::new(),
                },
                known_hosts,
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
            Self::start_with("", "")
        }

        pub fn http_addr(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], self.port))
        }

        pub fn ssh_addr(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], self.ssh_port))
        }

        pub fn log(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("serve.log")).unwrap_or_default()
        }

        fn wait_ready(&mut self) {
            let end = Instant::now() + Duration::from_secs(90);
            loop {
                if let Some(c) = self.child.as_mut()
                    && let Ok(Some(status)) = c.try_wait()
                {
                    panic!("asc serve exited early ({status}): {}", self.log());
                }
                let http_up =
                    TcpStream::connect_timeout(&self.http_addr(), Duration::from_millis(200))
                        .is_ok_and(|mut s| {
                            let _ = s.write_all(
                                b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                            );
                            let mut raw = String::new();
                            let _ = s.read_to_string(&mut raw);
                            raw.starts_with("HTTP/1.1 200")
                        });
                let ssh_up =
                    TcpStream::connect_timeout(&self.ssh_addr(), Duration::from_millis(200))
                        .is_ok();
                if http_up && (ssh_up || !cfg!(feature = "git-ssh")) {
                    break;
                }
                assert!(
                    Instant::now() < end,
                    "asc serve never listened: {}",
                    self.log()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            #[cfg(feature = "git-ssh")]
            self.seed_known_hosts();
        }

        /// Writes the `known_hosts` file from `asc serve ssh host-key show`.
        #[cfg(feature = "git-ssh")]
        fn seed_known_hosts(&self) {
            let out = self.serve(&["ssh", "host-key", "show"]);
            assert!(
                out.status.success(),
                "host-key show: {}",
                lossy(&out.stderr)
            );
            let text = lossy(&out.stdout);
            let line = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("known_hosts:"))
                .expect("a known_hosts line");
            let mut parts = line.split_whitespace();
            let _pattern = parts.next().unwrap();
            let rest: Vec<&str> = parts.collect();
            std::fs::write(
                &self.known_hosts,
                format!("[127.0.0.1]:{} {}\n", self.ssh_port, rest.join(" ")),
            )
            .unwrap();
        }

        pub fn serve(&self, args: &[&str]) -> std::process::Output {
            let mut cmd = Command::new(asc_exe());
            clean(&mut cmd, self.dir.path());
            cmd.arg("serve")
                .arg("--config")
                .arg(&self.cfg)
                .args(args)
                .current_dir(self.dir.path());
            cmd.output().expect("run asc serve")
        }

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

        /// `RepoAdmin init` through `asc api`, as the admin token.
        pub fn create_repo(&self, name: &str) {
            let url = format!("http://127.0.0.1:{}/{name}", self.port);
            let req = format!(r#"{{"repo":{{"name":"{name}"}},"init":{{}}}}"#);
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

        /// `ssh-key add`, which must succeed.
        pub fn add_user_key(&self, user: &str, key: &Key) {
            let r = self.ssh_key_add(&["--user", user], key);
            assert!(r.status.success(), "ssh-key add: {}", lossy(&r.stderr));
        }

        pub fn add_deploy_key(&self, repo: &str, write: bool, key: &Key) {
            let mut flags = vec!["--repo", repo, "--deploy"];
            if write {
                flags.push("--write");
            }
            let r = self.ssh_key_add(&flags, key);
            assert!(r.status.success(), "ssh-key add: {}", lossy(&r.stderr));
        }

        fn ssh_key_add(&self, flags: &[&str], key: &Key) -> std::process::Output {
            let pubkey = key.public.to_string_lossy().into_owned();
            let mut args = vec!["ssh-key", "add"];
            args.extend_from_slice(flags);
            args.extend_from_slice(&["--key-file", &pubkey]);
            self.serve(&args)
        }

        /// `ssh-key revoke` by the `SHA256:` fingerprint of `key`.
        pub fn revoke_key(&self, key: &Key) {
            let fp = fingerprint(key);
            let r = self.serve(&["ssh-key", "revoke", &fp]);
            assert!(r.status.success(), "ssh-key revoke: {}", lossy(&r.stderr));
        }

        pub fn url(&self, repo: &str) -> String {
            format!("ssh://git@127.0.0.1:{}/{repo}.git", self.ssh_port)
        }

        /// The GitHub-style URL (the port comes from the ssh command or the config).
        pub fn scp_url(&self, repo: &str) -> String {
            format!("git@127.0.0.1:{repo}.git")
        }

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

    /// The `SHA256:` fingerprint of a key, as `ssh-keygen -l` prints it.
    pub fn fingerprint(key: &Key) -> String {
        use sha2::{Digest, Sha256};
        let blob = b64_decode(&key.blob());
        let digest = Sha256::digest(&blob);
        format!("SHA256:{}", b64_nopad(&digest))
    }

    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn b64_nopad(input: &[u8]) -> String {
        let mut out = String::new();
        for c in input.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..=c.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    pub fn b64_decode(input: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0;
        for ch in input.bytes().filter(|b| *b != b'=') {
            let v = T.iter().position(|t| *t == ch).expect("base64") as u32;
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        out
    }

    /// A rig for the common cases: a server, git, the tools, and alice (write on `acme/*`) and
    /// bob (read on `acme/*`) each with an ed25519 key.
    pub struct Rig {
        pub tools: Tools,
        pub stack: Stack,
        pub git: Git,
        pub alice: Key,
        pub bob: Key,
        pub keys: tempfile::TempDir,
    }

    impl Rig {
        pub fn new(tools: Tools) -> Self {
            Self::with(tools, "", "")
        }

        pub fn with(tools: Tools, ssh: &str, extra: &str) -> Self {
            let stack = Stack::start_with(ssh, extra);
            let keys = tempfile::tempdir().unwrap();
            let alice = tools.key(keys.path(), "alice", "ed25519");
            let bob = tools.key(keys.path(), "bob", "ed25519");
            for repo in ["acme/web", "acme/lib"] {
                stack.create_repo(repo);
            }
            stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
            stack.token(&["--user", "bob", "--repo", "acme/*", "--read"]);
            stack.add_user_key("alice", &alice);
            stack.add_user_key("bob", &bob);
            Self {
                tools,
                stack,
                git: Git::new(),
                alice,
                bob,
                keys,
            }
        }

        /// `GIT_SSH_COMMAND` for `key`, with no `-p` (the URL carries the port).
        pub fn cmd(&self, key: &Key) -> String {
            self.tools
                .command(None, Some(key), &self.stack.known_hosts, &[])
        }

        /// As [`Rig::cmd`] with `-p` (for `git@host:path` URLs).
        pub fn cmd_p(&self, key: &Key) -> String {
            self.tools.command(
                Some(self.stack.ssh_port),
                Some(key),
                &self.stack.known_hosts,
                &[],
            )
        }

        /// Pushes a fixture of `n` commits to `repo` as alice and returns its path.
        pub fn seed(&self, name: &str, repo: &str, n: usize) -> PathBuf {
            let src = self.git.repo_with_commits(name, n);
            let r = self.git.ssh(
                &src,
                &self.cmd(&self.alice),
                &["push", "-q", &self.stack.url(repo), "master"],
            );
            assert!(r.ok, "seed push: {}", mask(&r.stderr));
            src
        }
    }
}

use h::*;

// =================================================================================== git cases

#[cfg(feature = "git-ssh")]
#[test]
fn clone_fetch_pull_and_push_over_ssh() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let (git, cmd) = (&rig.git, rig.cmd(&rig.alice));
    let url = rig.stack.url("acme/web");

    // An empty repository clones with git's own warning.
    let base = git.work("empty");
    let r = git.ssh(&base, &cmd, &["clone", &url, "e"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert!(r.stderr.contains("empty repository"), "{}", r.stderr);

    // Push: new branch.
    let a = rig.seed("a", "acme/web", 3);
    let sha = git.head(&a);
    let b = git.work("b");
    git.ssh_ok(&b, &cmd, &["clone", "-q", &url, "w"]);
    let bw = b.join("w");
    assert_eq!(git.head(&bw), sha, "the clone has the pushed commit");

    // Fast-forward push from the clone; pull --ff-only in the first repository.
    git.commit(&bw, "ff.txt", "ff\n", "ff");
    git.ssh_ok(&bw, &cmd, &["push", "-q", "origin", "master"]);
    git.ssh_ok(&a, &cmd, &["remote", "add", "origin", &url]);
    git.ssh_ok(&a, &cmd, &["pull", "-q", "--ff-only", "origin", "master"]);
    assert_eq!(git.head(&a), git.head(&bw));

    // A non-fast-forward push is rejected in git's words; --force succeeds.
    // A topic branch can be rewritten; the shared trunk refuses a forced rewrite.
    git.ok(&a, &["checkout", "-q", "-b", "topic"]);
    git.ssh_ok(&a, &cmd, &["push", "-q", "origin", "topic"]);
    git.ok(&a, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let r = git.ssh(&a, &cmd, &["push", "origin", "topic"]);
    assert!(!r.ok, "non-fast-forward");
    assert!(
        r.stderr.contains("rejected") && r.stderr.contains("fetch first")
            || r.stderr.contains("non-fast-forward"),
        "{}",
        mask(&r.stderr)
    );
    // The server's policy decides whether a rewrite is allowed; a refusal says why in a
    // `remote: asc:` line, and git's own output is otherwise untouched.
    let forced = git.ssh(&a, &cmd, &["push", "-q", "--force", "origin", "topic"]);
    if forced.ok {
        git.ssh_ok(&bw, &cmd, &["fetch", "-q", "origin"]);
        assert_eq!(
            git.ok(&bw, &["rev-parse", "origin/topic"]),
            git.head(&a),
            "fetch sees the forced update"
        );
    } else {
        assert!(
            forced.stderr.contains("remote: asc:"),
            "{}",
            mask(&forced.stderr)
        );
    }
    git.ssh_ok(&a, &cmd, &["push", "-q", "origin", ":topic"]);
    git.ok(&a, &["checkout", "-q", "master"]);
    git.ok(&a, &["reset", "-q", "--hard", &sha]);
    let r = git.ssh(&a, &cmd, &["push", "--force", "origin", "master"]);
    assert!(
        !r.ok && r.stderr.contains("remote:"),
        "a forced rewrite of a shared ref is refused: {}",
        mask(&r.stderr)
    );
    git.ssh_ok(&a, &cmd, &["pull", "-q", "--ff-only", "origin", "master"]);

    // Branch, tag, atomic push of two refs, delete.
    git.ok(&a, &["checkout", "-q", "-b", "feat"]);
    git.commit(&a, "feat.txt", "f\n", "feat");
    git.ok(&a, &["tag", "-a", "v1", "-m", "one"]);
    git.ssh_ok(&a, &cmd, &["push", "-q", "origin", "feat", "v1"]);
    git.ok(&a, &["checkout", "-q", "master"]);
    git.commit(&a, "m2.txt", "2\n", "m2");
    git.ok(&a, &["checkout", "-q", "feat"]);
    git.commit(&a, "feat2.txt", "f2\n", "feat2");
    git.ssh_ok(
        &a,
        &cmd,
        &["push", "-q", "--atomic", "origin", "master", "feat"],
    );
    let ls = git.ssh_ok(&a, &cmd, &["ls-remote", "origin"]);
    for want in ["refs/heads/master", "refs/heads/feat", "refs/tags/v1"] {
        assert!(ls.contains(want), "{want} in {ls}");
    }
    git.ssh_ok(&a, &cmd, &["push", "-q", "origin", ":feat"]);
    let ls = git.ssh_ok(&a, &cmd, &["ls-remote", "origin"]);
    assert!(!ls.contains("refs/heads/feat"), "deleted: {ls}");

    // --mirror into a second repository: every ref and sha equal.
    git.ok(&a, &["branch", "-q", "extra", "master"]);
    let msrc = git.work("msrc");
    git.ok(&msrc, &["init", "-q", "--bare", "m.git"]);
    git.ok(
        &a,
        &[
            "push",
            "-q",
            &fwd(&msrc.join("m.git")),
            "refs/heads/*:refs/heads/*",
            "refs/tags/*:refs/tags/*",
        ],
    );
    git.ssh_ok(
        &msrc.join("m.git"),
        &cmd,
        &["push", "-q", "--mirror", &rig.stack.url("acme/lib")],
    );
    let back = git.work("back");
    git.ssh_ok(
        &back,
        &cmd,
        &[
            "clone",
            "-q",
            "--mirror",
            &rig.stack.url("acme/lib"),
            "m.git",
        ],
    );
    assert_eq!(git.refs(&back.join("m.git")), git.refs(&msrc.join("m.git")));

    // A read-only key fetches and cannot push.
    let rcmd = rig.cmd(&rig.bob);
    let ro = git.work("ro");
    git.ssh_ok(&ro, &rcmd, &["clone", "-q", &url, "w"]);
    git.commit(&ro.join("w"), "x.txt", "x\n", "x");
    let r = git.ssh(&ro.join("w"), &rcmd, &["push", "origin", "master"]);
    assert!(!r.ok, "a read-only key must not push");
    assert!(r.stderr.contains("read-only access"), "{}", mask(&r.stderr));
}

#[cfg(feature = "git-ssh")]
#[test]
fn shallow_and_partial_clones_over_ssh() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let (git, cmd) = (&rig.git, rig.cmd(&rig.alice));
    let url = rig.stack.url("acme/web");
    let src = rig.seed("src", "acme/web", 6);

    let base = git.work("shallow");
    git.ssh_ok(&base, &cmd, &["clone", "-q", "--depth", "1", &url, "s"]);
    let s = base.join("s");
    assert_eq!(git.ok(&s, &["rev-list", "--count", "HEAD"]), "1");
    git.ssh_ok(&s, &cmd, &["fetch", "-q", "--deepen", "2"]);
    assert_eq!(git.ok(&s, &["rev-list", "--count", "HEAD"]), "3");
    git.ssh_ok(&s, &cmd, &["fetch", "-q", "--unshallow"]);
    assert_eq!(git.ok(&s, &["rev-list", "--count", "HEAD"]), "6");
    assert_eq!(git.head(&s), git.head(&src));

    // Partial clones, then the lazy fetch of a missing blob.
    for (name, filter) in [
        ("blob", "--filter=blob:none"),
        ("lim", "--filter=blob:limit=1"),
    ] {
        let p = git.work(&format!("partial-{name}"));
        git.ssh_ok(
            &p,
            &cmd,
            &["clone", "-q", filter, "--no-checkout", &url, "w"],
        );
        let w = p.join("w");
        let r = git.ssh(&w, &cmd, &["checkout", "-q", "master"]);
        assert!(r.ok, "lazy fetch on checkout: {}", mask(&r.stderr));
        assert_eq!(
            std::fs::read_to_string(w.join("f3.txt")).unwrap().trim(),
            "3"
        );
    }
}

#[cfg(feature = "git-ssh")]
#[test]
fn protocol_versions_zero_one_and_two() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let (git, cmd) = (&rig.git, rig.cmd(&rig.alice));
    let url = rig.stack.url("acme/web");
    let src = rig.seed("src", "acme/web", 2);
    let sha = git.head(&src);
    let cwd = git.work("lsr");
    let mut seen = Vec::new();
    for v in ["0", "1", "2"] {
        let ls = git.ssh_ok(
            &cwd,
            &cmd,
            &["-c", &format!("protocol.version={v}"), "ls-remote", &url],
        );
        assert!(
            ls.contains(&format!("{sha}\trefs/heads/master")),
            "v{v}: {ls}"
        );
        let c = git.work(&format!("c{v}"));
        git.ssh_ok(
            &c,
            &cmd,
            &[
                "-c",
                &format!("protocol.version={v}"),
                "clone",
                "-q",
                &url,
                "w",
            ],
        );
        assert_eq!(git.head(&c.join("w")), sha, "v{v} clone");
        seen.push(ls);
    }
    // Version 1 adds a `version 1` banner that ls-remote hides: the ref lines agree.
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
}

// ======================================================================= the build without git-ssh

/// `scripts/ci/no-git-compat.sh` runs this against a `git-compat` build without `git-ssh`:
/// `[ssh] enabled = true` binds no SSH port and the server says why.
#[cfg(not(feature = "git-ssh"))]
#[test]
fn a_git_compat_build_without_git_ssh_binds_no_ssh_port() {
    let mut stack = Stack::start();
    // The one-line notice goes to `tracing`, which `asc serve` has no subscriber for yet, so the
    // absence of the port is what can be observed.
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(stack.alive(), "{}", stack.log());
    assert!(
        std::net::TcpStream::connect_timeout(
            &stack.ssh_addr(),
            std::time::Duration::from_millis(500)
        )
        .is_err(),
        "no SSH port may be bound without the feature"
    );
}
