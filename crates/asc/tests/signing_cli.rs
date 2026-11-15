//! `asc signing default` against a stand-in `Signing` server on loopback: it reads and sets the account's
//! sign-by-default switch, a set carries an operation id, the value is kept in the global config as
//! `asc.signByDefault`, a local `commit.gpgsign` is reported as winning, and a bad value never leaves the machine.

mod support;

use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asc_rpc::server::wire;
use asc_rpc::services::signing_server::{Signing, SigningServer};
use support::{Sandbox, text};
use tonic::{Request, Response, Status};

const TOKEN: &str = "secret-token-value";

#[derive(Default)]
struct State {
    on: bool,
    updated: i64,
    gets: usize,
    sets: Vec<(bool, Option<String>)>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<State>>);

fn op_of<T>(r: &Request<T>) -> Option<String> {
    r.metadata()
        .get("x-asc-op-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[tonic::async_trait]
impl Signing for Fake {
    async fn list_keys(
        &self,
        _: Request<wire::ListSigningKeysRequest>,
    ) -> Result<Response<wire::ListSigningKeysResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn add_key(
        &self,
        _: Request<wire::AddSigningKeyRequest>,
    ) -> Result<Response<wire::AddSigningKeyResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn revoke_key(
        &self,
        _: Request<wire::RevokeSigningKeyRequest>,
    ) -> Result<Response<wire::RevokeSigningKeyResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn submit_signature(
        &self,
        _: Request<wire::SubmitSignatureRequest>,
    ) -> Result<Response<wire::SubmitSignatureResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn verify(
        &self,
        _: Request<wire::VerifySignaturesRequest>,
    ) -> Result<Response<wire::VerifySignaturesResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn get_server_key(
        &self,
        _: Request<wire::GetServerKeyRequest>,
    ) -> Result<Response<wire::GetServerKeyResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn get_signing_defaults(
        &self,
        r: Request<wire::GetSigningDefaultsRequest>,
    ) -> Result<Response<wire::GetSigningDefaultsResponse>, Status> {
        assert!(
            r.get_ref().user.is_empty(),
            "the CLI asks for its own account"
        );
        let mut s = self.0.lock().unwrap();
        s.gets += 1;
        Ok(Response::new(wire::GetSigningDefaultsResponse {
            sign_by_default: s.on,
            updated: s.updated,
        }))
    }
    async fn set_signing_defaults(
        &self,
        r: Request<wire::SetSigningDefaultsRequest>,
    ) -> Result<Response<wire::SetSigningDefaultsResponse>, Status> {
        let op = op_of(&r);
        let mut s = self.0.lock().unwrap();
        s.on = r.get_ref().sign_by_default;
        s.updated = 1_794_000_000;
        let on = s.on;
        s.sets.push((on, op));
        Ok(Response::new(wire::SetSigningDefaultsResponse {
            sign_by_default: s.on,
            updated: s.updated,
        }))
    }
}

struct Server {
    addr: std::net::SocketAddr,
    fake: Fake,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let fake = Fake::default();
        let svc = fake.clone();
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
                let _ = tonic::transport::Server::builder()
                    .add_service(SigningServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self {
            addr: addr_rx.recv().unwrap(),
            fake,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
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

fn run(sb: &Sandbox, cwd: &Path, server: &Server, pre: &[&str], args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.env("ASC_TOKEN", TOKEN).env("ASC_NO_DAEMON", "1");
    cmd.args(pre).args(args).args(["--remote", &server.url()]);
    cmd.output().expect("spawn asc")
}

fn ok(out: &Output) -> String {
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {se}");
    assert!(
        !format!("{so}{se}").contains(TOKEN),
        "the token was printed"
    );
    so
}

fn global(sb: &Sandbox) -> String {
    // Keys are case-insensitive; the writer keeps them lower case.
    std::fs::read_to_string(sb.user.join(".gitconfig"))
        .unwrap()
        .to_ascii_lowercase()
}

#[test]
fn signing_default_reads_sets_and_keeps_the_account_switch() {
    let server = Server::start();
    let sb = Sandbox::new();
    let cwd = sb.dir("w");

    let so = ok(&run(&sb, &cwd, &server, &[], &["signing", "default"]));
    assert_eq!(so.trim(), "sign by default: off");
    assert!(
        global(&sb).contains("signbydefault = false"),
        "{}",
        global(&sb)
    );

    let so = ok(&run(&sb, &cwd, &server, &[], &["signing", "default", "on"]));
    assert_eq!(so.trim(), "sign by default: on");
    assert!(
        global(&sb).contains("signbydefault = true"),
        "{}",
        global(&sb)
    );
    {
        let s = server.fake.0.lock().unwrap();
        assert_eq!(s.sets.len(), 1);
        assert!(s.sets[0].0);
        assert!(
            s.sets[0].1.as_deref().is_some_and(|o| !o.is_empty()),
            "a set carries an operation id"
        );
    }

    let so = ok(&run(
        &sb,
        &cwd,
        &server,
        &[],
        &["signing", "default", "--json"],
    ));
    let v: serde_json::Value = serde_json::from_str(&so).unwrap();
    assert_eq!(v["sign_by_default"], true, "{so}");

    // A local commit.gpgsign that disagrees is named as the winner.
    let so = ok(&run(
        &sb,
        &cwd,
        &server,
        &["-c", "commit.gpgsign=false"],
        &["signing", "default"],
    ));
    assert!(so.contains("commit.gpgsign is false here and wins"), "{so}");

    let so = ok(&run(
        &sb,
        &cwd,
        &server,
        &[],
        &["signing", "default", "off"],
    ));
    assert_eq!(so.trim(), "sign by default: off");
    assert!(global(&sb).contains("signbydefault = false"));
    assert_eq!(server.fake.0.lock().unwrap().gets, 3);
}

#[test]
fn a_bad_signing_value_never_leaves_the_machine() {
    let server = Server::start();
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    for bad in ["maybe", "ON ", "1x"] {
        let out = run(&sb, &cwd, &server, &[], &["signing", "default", bad]);
        assert_ne!(out.status.code(), Some(0));
        assert!(
            text(&out.stderr).contains("on or off"),
            "{}",
            text(&out.stderr)
        );
    }
    let s = server.fake.0.lock().unwrap();
    assert_eq!((s.gets, s.sets.len()), (0, 0));
    assert!(!global(&sb).contains("signbydefault"));
}

// ---- `asc signing status` and `asc signing test`: local config only, no server. ----

const KEY_BODY: &str = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBEHO7rHJ3AV+cNx9RP5Y4LKRX3Ny/ywRZ9d2Sd7WVvxgAAAJDe4Cnq3uAp
6gAAAAtzc2gtZWQyNTUxOQAAACBEHO7rHJ3AV+cNx9RP5Y4LKRX3Ny/ywRZ9d2Sd7WVvxg
AAAECyVlI1ETu9uLtTIPfbajG0zVEMdAsj8RUn6qUZSM/pqEQc7uscncBX5w3H1E/ljgsp
Ffc3L/LBFn13ZJ3tZW/GAAAACWVkX2FAdGVzdAECAwQ=";

fn signing(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .env("ASC_NO_DAEMON", "1")
        .args(["signing"])
        .args(args)
        .output()
        .expect("spawn asc")
}

fn write_key(sb: &Sandbox) -> String {
    let path = sb.user.join("id_test");
    std::fs::write(
        &path,
        format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{KEY_BODY}\n-----END OPENSSH PRIVATE KEY-----\n"
        ),
    )
    .unwrap();
    // ssh-keygen refuses a private key that others can read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path.display().to_string().replace('\\', "/")
}

fn set_global(sb: &Sandbox, cwd: &Path, key: &str, value: &str) {
    let out = sb.run(cwd, &["config", "--global", key, value]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

#[test]
fn signing_status_with_no_config_is_unconfigured() {
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let out = signing(&sb, &cwd, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(v["configured"], false);
    assert_eq!(v["key"]["source"], "none");
    assert_eq!(v["format"]["value"], "openpgp");
    let human = text(&signing(&sb, &cwd, &["status"]).stdout);
    assert!(human.contains("key: none"), "{human}");
}

#[test]
fn signing_status_and_test_with_an_ssh_key_file() {
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let key = write_key(&sb);
    set_global(&sb, &cwd, "user.signingkey", &key);
    set_global(&sb, &cwd, "gpg.format", "ssh");
    set_global(&sb, &cwd, "commit.gpgsign", "true");

    let out = signing(&sb, &cwd, &["status", "--json"]);
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&so).unwrap();
    assert_eq!(v["key"]["source"], "file");
    assert_eq!(v["key"]["path"], key.as_str());
    assert_eq!(v["key"]["usable"], true);
    assert_eq!(v["key"]["level"], "global");
    assert_eq!(v["format"]["value"], "ssh");
    assert_eq!(v["commit_sign"]["value"], true);
    assert_eq!(v["commit_sign"]["level"], "global");
    assert!(
        v["key"]["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("SHA256:")
    );
    for line in KEY_BODY.lines() {
        assert!(!so.contains(line), "private key text in the JSON");
    }

    let out = signing(&sb, &cwd, &["test", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let t: serde_json::Value = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(t["ok"], true);
    assert_eq!(t["fingerprint"], v["key"]["fingerprint"]);
    let human = text(&signing(&sb, &cwd, &["test"]).stdout);
    assert!(
        human.starts_with("ok: signed and verified, key SHA256:"),
        "{human}"
    );
}

#[test]
fn signing_test_with_an_unusable_key_exits_1() {
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let out = signing(&sb, &cwd, &["test"]);
    assert_eq!(out.status.code(), Some(1), "not configured");
    assert!(
        text(&out.stdout).starts_with("failed:"),
        "{}",
        text(&out.stdout)
    );

    set_global(&sb, &cwd, "gpg.format", "ssh");
    set_global(&sb, &cwd, "user.signingkey", "/no/such/key");
    let out = signing(&sb, &cwd, &["test", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let t: serde_json::Value = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(t["ok"], false);
    assert!(t["error"].as_str().unwrap().contains("cannot be read"));
}

/// The commit object text of `HEAD` split into the signature (armor lines, unfolded) and the payload.
fn split_signed(raw: &str) -> (String, String) {
    let mut sig = String::new();
    let mut payload = String::new();
    let mut in_sig = false;
    for line in raw.split_inclusive('\n') {
        if let Some(first) = line.strip_prefix("gpgsig ") {
            in_sig = true;
            sig.push_str(first);
        } else if in_sig && line.starts_with(' ') {
            sig.push_str(&line[1..]);
        } else {
            in_sig = false;
            payload.push_str(line);
        }
    }
    (sig, payload)
}

fn have_ssh_keygen() -> bool {
    std::process::Command::new("ssh-keygen")
        .arg("-?")
        .output()
        .is_ok()
}

#[test]
fn a_commit_signs_with_gpg_format_ssh_and_a_key_file() {
    if !have_ssh_keygen() {
        return;
    }
    use std::io::Write as _;
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    let key = write_key(&sb);
    set_global(&sb, &cwd, "user.signingkey", &key);
    set_global(&sb, &cwd, "gpg.format", "ssh");
    set_global(&sb, &cwd, "user.name", "T");
    set_global(&sb, &cwd, "user.email", "t@example.com");
    // A `gpg` that always fails: the commit must not reach it.
    set_global(&sb, &cwd, "gpg.program", "false");
    let step = |args: &[&str]| {
        let out = sb.run(&cwd, args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
        text(&out.stdout)
    };
    step(&["init", "-q"]);
    std::fs::write(cwd.join("f.txt"), "one\n").unwrap();
    step(&["add", "f.txt"]);
    step(&["commit", "-q", "-S", "-m", "signed"]);
    let (sig, payload) = split_signed(&step(&["cat-file", "-p", "HEAD"]));
    assert!(sig.starts_with("-----BEGIN SSH SIGNATURE-----"), "{sig}");
    let sigf = sb.user.join("commit.sig");
    std::fs::write(&sigf, &sig).unwrap();
    let mut check = std::process::Command::new("ssh-keygen")
        .args(["-Y", "check-novalidate", "-n", "git", "-s"])
        .arg(&sigf)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    check
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = check.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let fp = signing(&sb, &cwd, &["status", "--json"]).stdout;
    let v: serde_json::Value = serde_json::from_str(&text(&fp)).unwrap();
    assert!(text(&out.stdout).contains(v["key"]["fingerprint"].as_str().unwrap()));
    // A tag signs the same way.
    step(&["tag", "-s", "-m", "t", "v1"]);
}

#[cfg(unix)]
#[test]
fn signing_refuses_a_fifo_a_device_a_directory_and_an_oversized_key_file() {
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    set_global(&sb, &cwd, "gpg.format", "ssh");
    let big = sb.user.join("big");
    std::fs::write(&big, vec![b'a'; 64 * 1024 + 1]).unwrap();
    let fifo = sb.user.join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let dir = sb.dir("keydir");
    for p in [
        "/dev/zero".to_owned(),
        big.display().to_string(),
        fifo.display().to_string(),
        dir.display().to_string(),
    ] {
        set_global(&sb, &cwd, "user.signingkey", &p);
        let out = signing(&sb, &cwd, &["test", "--json"]);
        assert_eq!(out.status.code(), Some(1), "{p}");
        let t: serde_json::Value = serde_json::from_str(&text(&out.stdout)).unwrap();
        let e = t["error"].as_str().unwrap();
        assert!(
            e.contains("not a regular file") || e.contains("larger than 64 KiB"),
            "{p}: {e}"
        );
    }
}
