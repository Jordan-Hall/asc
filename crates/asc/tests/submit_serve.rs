//! `asc submit` against the real `asc serve` binary (token store, SQLite, in-memory objects) with
//! the real `asc` client: a repository with a trunk, three stacked changes, `asc submit -s`, a
//! fourth change, `asc submit -s` again.
//!
//! This is the product's core flow end to end. The unit tests of the submit handler wrote the
//! change index directly and `stack_submit.rs` answers `UpdateRefs` with a mock, so nothing ever
//! showed that the `m/` change metadata a client uploads reached the change index of a real
//! server: every real `asc submit` failed with "the changeset is not a version of the change its
//! topic names".
#![cfg(feature = "serve")]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

mod support;
use support::{Sandbox, text};

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

/// The process environment of a child of this test: nothing of the developer's `GIT_` or `ASC_`
/// variables, and a home of its own.
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

/// A running `asc serve`; dropping it kills the process.
struct Server {
    dir: tempfile::TempDir,
    port: u16,
    admin: String,
    child: Option<Child>,
    cfg: PathBuf,
}

impl Server {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let admin_port = free_port();
        let cfg = dir.path().join("asc-serve.toml");
        std::fs::write(
            &cfg,
            format!(
                "listen = \"127.0.0.1:{port}\"\npublic_url = \"http://127.0.0.1:{port}\"\n\
                 store = \"memory:///\"\n\
                 db = \"sqlite:{db}?mode=rwc\"\n\
                 cache = {{ disk_dir = {cache:?}, disk = \"1GiB\" }}\n\
                 [auth]\nprovider = \"token_store\"\ntoken_key_file = {key:?}\n\
                 [admin]\nlisten = \"127.0.0.1:{admin_port}\"\n",
                db = fwd(&dir.path().join("asc.db")),
                cache = fwd(&dir.path().join("cache")),
                key = fwd(&dir.path().join("token.key")),
            ),
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("asc-home")).unwrap();
        let mut s = Self {
            dir,
            port,
            admin: String::new(),
            child: None,
            cfg,
        };
        let m = s.serve(&["migrate"]);
        assert!(m.status.success(), "migrate: {}", text(&m.stderr));
        s.admin = s.token(&["--user", "root", "--repo", "acme/*", "--admin"]);
        let log = std::fs::File::create(s.dir.path().join("serve.log")).unwrap();
        let mut cmd = Command::new(asc_exe());
        clean(&mut cmd, s.dir.path());
        cmd.arg("serve")
            .arg("--config")
            .arg(&s.cfg)
            .current_dir(s.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        s.child = Some(cmd.spawn().expect("spawn asc serve"));
        s.wait_ready();
        s
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    fn log(&self) -> String {
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
            if TcpStream::connect_timeout(&self.addr(), Duration::from_millis(200)).is_ok() {
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

    fn serve(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(asc_exe());
        clean(&mut cmd, self.dir.path());
        cmd.arg("serve")
            .arg("--config")
            .arg(&self.cfg)
            .args(args)
            .current_dir(self.dir.path());
        cmd.output().expect("run asc serve")
    }

    fn token(&self, flags: &[&str]) -> String {
        let mut args = vec!["token", "create"];
        args.extend_from_slice(flags);
        let out = self.serve(&args);
        assert!(out.status.success(), "token create: {}", text(&out.stderr));
        text(&out.stdout).trim().to_owned()
    }

    fn url(&self, repo: &str) -> String {
        format!("http://127.0.0.1:{}/{repo}", self.port)
    }

    fn create_repo(&self, name: &str) {
        let req = format!(r#"{{"repo":{{"name":"{name}"}},"init":{{}}}}"#);
        let mut cmd = Command::new(asc_exe());
        clean(&mut cmd, self.dir.path());
        let out = cmd
            .args([
                "api",
                "RepoAdmin",
                "--remote",
                &self.url(name),
                "--json",
                &req,
            ])
            .env("ASC_TOKEN", &self.admin)
            .current_dir(self.dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "RepoAdmin init {name}: {}",
            text(&out.stderr)
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct Client<'a> {
    sb: &'a Sandbox,
    dir: PathBuf,
    token: &'a str,
}

impl Client<'_> {
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = self.sb.asc(&self.dir);
        cmd.envs(IDENT).env("ASC_TOKEN", self.token).args(args);
        cmd.output().expect("spawn asc")
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "asc {args:?} failed: {}",
            text(&out.stderr)
        );
        out
    }

    fn commit_file(&self, name: &str, body: &str) {
        let path = self.dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        self.ok(&["add", name]);
        self.ok(&["commit", "-q", "-m", &format!("add {name}")]);
    }
}

#[test]
fn a_stack_of_three_is_submitted_and_a_fourth_change_extends_it() {
    let server = Server::start();
    server.create_repo("acme/web");
    let sb = Sandbox::new();
    let dir = sb.dir("work");
    let c = Client {
        sb: &sb,
        dir,
        token: &server.admin,
    };

    c.ok(&["init", "-q"]);
    c.ok(&["remote", "add", "origin", &server.url("acme/web")]);
    c.commit_file("base.txt", "base\n");
    c.ok(&["push", "origin", "master"]);
    c.ok(&["config", "asc.stack.trunk", "master"]);

    for n in 1..=3 {
        c.commit_file(&format!("step{n}.txt"), &format!("step {n}\n"));
    }
    let first = c.run(&["submit", "-s", "--json"]);
    assert!(
        first.status.success(),
        "the first submit failed: {}\nserver: {}",
        text(&first.stderr),
        server.log()
    );
    let json: serde_json::Value = serde_json::from_str(text(&first.stdout).trim()).unwrap();
    assert_eq!(json["status"], "submitted", "{json}");
    assert_eq!(json["changes"].as_array().unwrap().len(), 3, "{json}");

    // The same stack again changes nothing; one more change goes up on top of it.
    c.commit_file("step4.txt", "step 4\n");
    let second = c.run(&["submit", "-s", "--json"]);
    assert!(
        second.status.success(),
        "the second submit failed: {}\nserver: {}",
        text(&second.stderr),
        server.log()
    );
    let json: serde_json::Value = serde_json::from_str(text(&second.stdout).trim()).unwrap();
    assert_eq!(json["status"], "submitted", "{json}");
    assert_eq!(json["changes"].as_array().unwrap().len(), 4, "{json}");
}

/// A `push` to the trunk that the policy refuses leaves the commit on the local trunk branch. The next
/// `asc submit -s` must still submit the stack: the refused commit is part of what the person made, and it
/// has to reach the server as a change like any other, not break the metadata of the stack.
#[test]
fn a_submit_after_a_push_the_policy_refused_still_submits_the_stack() {
    let server = Server::start();
    server.create_repo("acme/web");
    let sb = Sandbox::new();
    let admin = Client {
        sb: &sb,
        dir: sb.dir("admin"),
        token: &server.admin,
    };
    admin.ok(&["init", "-q"]);
    admin.ok(&["remote", "add", "origin", &server.url("acme/web")]);
    admin.commit_file("base.txt", "base\n");
    admin.commit_file(
        ".asc/policy.toml",
        "version = 1\n\n[roles]\nmaintainer = [\"user:root\"]\n\n[trunk]\nrequire_approvals = 1\nallow_force = false\n",
    );
    admin.ok(&["push", "origin", "master"]);

    // A token principal is no user the byte ledger can name, so only an administrator may
    // publish a changeset it did not bring with a submit; the policy still binds Bob.
    let bob_token = server.token(&["--user", "bob", "--repo", "acme/*", "--admin"]);
    let bob = Client {
        sb: &sb,
        dir: sb.dir("bob"),
        token: &bob_token,
    };
    bob.ok(&["init", "-q"]);
    bob.ok(&["remote", "add", "origin", &server.url("acme/web")]);
    bob.ok(&["pull", "origin", "master"]);
    bob.ok(&["config", "asc.stack.trunk", "master"]);
    bob.commit_file("direct.txt", "direct\n");
    let refused = bob.run(&["push", "origin", "master"]);
    assert!(
        !refused.status.success(),
        "a direct push to the trunk was accepted: {}",
        text(&refused.stdout)
    );

    bob.commit_file("step1.txt", "step 1\n");
    let out = bob.run(&["submit", "-s", "--json"]);
    assert!(
        out.status.success(),
        "the submit after the refused push failed: {}\nserver: {}",
        text(&out.stderr),
        server.log()
    );
}

/// A plain push of a branch the server keeps sends the change metadata too, so a later submit
/// of the same commits finds their changes on the server.
#[test]
fn a_submit_after_a_plain_push_of_the_same_commits_submits_the_stack() {
    let server = Server::start();
    server.create_repo("acme/web");
    let sb = Sandbox::new();
    let admin = Client {
        sb: &sb,
        dir: sb.dir("admin"),
        token: &server.admin,
    };
    admin.ok(&["init", "-q"]);
    admin.ok(&["remote", "add", "origin", &server.url("acme/web")]);
    admin.commit_file("base.txt", "base\n");
    admin.ok(&["push", "origin", "master"]);
    admin.ok(&["config", "asc.stack.trunk", "master"]);

    admin.commit_file("one.txt", "one\n");
    admin.commit_file("two.txt", "two\n");
    admin.ok(&["push", "origin", "HEAD:refs/heads/side"]);
    let out = admin.run(&["submit", "-s", "--json"]);
    assert!(
        out.status.success(),
        "the submit after a plain push failed: {}\nserver: {}",
        text(&out.stderr),
        server.log()
    );
}
