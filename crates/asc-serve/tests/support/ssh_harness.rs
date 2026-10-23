//! A listener on a loopback port, a fake authorizer, a recording audit sink and a recording
//! `PackExec`, plus `russh` client helpers (PLAN U86 tests).
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Decision, Principal, SshFingerprint, SshIdentity,
    SshKey, SshKeyKind,
};
use asc_serve::config::{Limits, SshConfig, SshLimitsConfig};
use asc_serve::limits::Admission;
use asc_serve::ssh::audit::{CommandRecord, LoginRecord, RefusedRecord};
use asc_serve::ssh::{
    Audit, AuditSink, PackExec, PackIo, PackRequest, SshDeps, SshMetrics, SshServer,
};
use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{Channel, ChannelMsg};

/// Everything a test waits for is bounded.
pub async fn within<T>(secs: u64, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), f)
        .await
        .expect("timed out")
}

/// Polls `cond` until it holds (5 s at most).
pub async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never true: {what}");
}

pub fn private(pem: &str) -> Arc<PrivateKey> {
    Arc::new(PrivateKey::from_openssh(pem.trim()).expect("test key"))
}

pub fn public(line: &str) -> PublicKey {
    PublicKey::from_openssh(line).expect("test public key")
}

pub fn fingerprint(line: &str) -> SshFingerprint {
    SshFingerprint::of_blob(&public(line).to_bytes().unwrap())
}

// ------------------------------------------------------------------------------ the authorizer

/// Knows the keys a test registers. Every call is counted.
#[derive(Default)]
pub struct FakeAuthz {
    keys: Mutex<HashMap<SshFingerprint, SshIdentity>>,
    pub fail_identify: AtomicBool,
    pub identify_calls: AtomicUsize,
    pub authorize_calls: AtomicUsize,
}

impl FakeAuthz {
    pub fn add_user(&self, line: &str, user: &str) {
        self.add(line, "user", SshKeyKind::User, &format!("pat:{user}"), None);
    }

    pub fn add(&self, line: &str, id: &str, kind: SshKeyKind, principal: &str, repo: Option<&str>) {
        self.keys.lock().unwrap().insert(
            fingerprint(line),
            SshIdentity {
                key_id: id.into(),
                kind,
                principal: Principal {
                    id: principal.into(),
                },
                repo: repo.map(str::to_owned),
            },
        );
    }

    pub fn revoke(&self, line: &str) {
        self.keys.lock().unwrap().remove(&fingerprint(line));
    }
}

#[async_trait]
impl Authorizer for FakeAuthz {
    async fn authorize(
        &self,
        _token: Option<&BearerToken>,
        _repo: &str,
        _access: Access,
    ) -> Result<Principal, AuthError> {
        Err(AuthError::Unauthenticated)
    }

    async fn identify_ssh_key(&self, key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        self.identify_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_identify.load(Ordering::SeqCst) {
            return Err(AuthError::Unauthenticated);
        }
        Ok(self.keys.lock().unwrap().get(&key.fingerprint()).cloned())
    }

    async fn authorize_ssh_key(
        &self,
        key: &SshKey,
        _repo: &str,
        _access: Access,
    ) -> Result<Decision, AuthError> {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        Ok(match self.keys.lock().unwrap().get(&key.fingerprint()) {
            Some(id) => Decision::Allowed(id.principal.clone()),
            None => Decision::Denied,
        })
    }
}

// ------------------------------------------------------------------------------ audit

/// Keeps every record.
#[derive(Default)]
pub struct Records {
    logins: Mutex<Vec<LoginRecord>>,
    refused: Mutex<Vec<RefusedRecord>>,
    commands: Mutex<Vec<CommandRecord>>,
}

impl Records {
    pub fn logins(&self) -> Vec<LoginRecord> {
        self.logins.lock().unwrap().clone()
    }

    pub fn outcomes(&self) -> Vec<String> {
        self.logins().iter().map(|r| r.outcome.label()).collect()
    }

    pub fn refused(&self) -> Vec<RefusedRecord> {
        self.refused.lock().unwrap().clone()
    }

    pub fn refused_names(&self) -> Vec<&'static str> {
        self.refused().iter().map(|r| r.request).collect()
    }

    pub fn commands(&self) -> Vec<CommandRecord> {
        self.commands.lock().unwrap().clone()
    }

    /// Every record as text, for the "no key material" checks.
    pub fn debug_text(&self) -> String {
        format!(
            "{:?} {:?} {:?}",
            self.logins(),
            self.refused(),
            self.commands()
        )
    }
}

impl AuditSink for Records {
    fn login(&self, r: &LoginRecord) {
        self.logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(r.clone());
    }

    fn refused(&self, r: &RefusedRecord) {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(r.clone());
    }

    fn command(&self, r: &CommandRecord) {
        self.commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(r.clone());
    }
}

// ------------------------------------------------------------------------------ the exec

/// What the recording exec does with a command.
#[derive(Clone)]
pub enum Mode {
    /// Writes this on stdout and exits with the code.
    Reply(&'static [u8], u32),
    /// Echoes its input to stdout until EOF, then exits 0.
    Echo,
    /// Never finishes; counts when it is dropped (the server killed it).
    Hang,
}

/// What a command looked like to the exec.
#[derive(Clone, Debug)]
pub struct Seen {
    pub verb: &'static str,
    pub repo: String,
    pub git_protocol: Option<String>,
    pub principal: String,
    pub fingerprint: SshFingerprint,
    pub key_kind: &'static str,
}

pub struct Recorder {
    pub mode: Mutex<Mode>,
    pub seen: Mutex<Vec<Seen>>,
    pub started: AtomicUsize,
    pub dropped: Arc<AtomicUsize>,
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl PackExec for Recorder {
    async fn run(&self, req: PackRequest, mut io: PackIo) -> u32 {
        let _count = DropCount(self.dropped.clone());
        // The exec asks the same authorizer, as U87's will.
        let _ = req
            .authorizer
            .authorize_ssh_key(&req.key, &req.repo, req.verb.access())
            .await;
        self.seen.lock().unwrap().push(Seen {
            verb: req.verb.as_str(),
            repo: req.repo.clone(),
            git_protocol: req.git_protocol.as_ref().map(|p| p.as_str().to_owned()),
            principal: req.principal.id.clone(),
            fingerprint: req.key.fingerprint(),
            key_kind: req.identity.kind.as_str(),
        });
        self.started.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode.lock().unwrap().clone();
        match mode {
            Mode::Reply(text, code) => {
                io.output.stdout(Bytes::from_static(text)).await;
                code
            }
            Mode::Echo => {
                while let Some(chunk) = io.input.recv().await {
                    io.output.stdout(chunk).await;
                }
                0
            }
            Mode::Hang => std::future::pending().await,
        }
    }
}

// ------------------------------------------------------------------------------ the server

pub struct Harness {
    pub addr: SocketAddr,
    pub authz: Arc<FakeAuthz>,
    pub records: Arc<Records>,
    pub metrics: Arc<SshMetrics>,
    pub exec: Arc<Recorder>,
    pub admission: Arc<Admission>,
    pub host_fingerprint: SshFingerprint,
    pub host_key_file: std::path::PathBuf,
    dir: Option<tempfile::TempDir>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// Stops the listener and hands back the directory, host key included (for a restart).
    pub fn stop(mut self) -> tempfile::TempDir {
        self.task.abort();
        self.dir.take().expect("directory")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Test limits: no rejection delay, everything else the SPEC default.
pub fn test_limits() -> SshLimitsConfig {
    SshLimitsConfig {
        reject_delay_ms: 0,
        ..SshLimitsConfig::default()
    }
}

pub async fn start() -> Harness {
    start_with(|_, _| {}, Limits::default()).await
}

pub async fn start_with(
    tune: impl FnOnce(&mut SshConfig, &mut SshLimitsConfig),
    shared: Limits,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    start_in(dir, tune, shared).await
}

/// As [`start_with`], on the host key file already in `dir` (a restart).
pub async fn start_in(
    dir: tempfile::TempDir,
    tune: impl FnOnce(&mut SshConfig, &mut SshLimitsConfig),
    shared: Limits,
) -> Harness {
    let host_key_file = dir.path().join("ssh_host_ed25519_key");
    let mut ssh = SshConfig {
        enabled: true,
        listen: "127.0.0.1:0".parse().unwrap(),
        host_key_files: vec![host_key_file.clone()],
        ..SshConfig::default()
    };
    let mut limits = test_limits();
    tune(&mut ssh, &mut limits);
    let authz = Arc::new(FakeAuthz::default());
    let records = Arc::new(Records::default());
    let metrics = SshMetrics::detached();
    let exec = Arc::new(Recorder {
        mode: Mutex::new(Mode::Reply(b"", 0)),
        seen: Mutex::default(),
        started: AtomicUsize::new(0),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let admission = Arc::new(Admission::new(&shared));
    let server = SshServer::bind(
        &ssh,
        &limits,
        SshDeps {
            authorizer: authz.clone(),
            admission: Some(admission.clone()),
            audit: Audit::new(records.clone(), metrics.clone()),
            exec: exec.clone(),
        },
    )
    .await
    .expect("bind");
    let addr = server.local_addr().unwrap();
    let host_fingerprint = server.host_keys().keys()[0].fingerprint();
    let task = tokio::spawn(server.run(std::future::pending()));
    Harness {
        addr,
        authz,
        records,
        metrics,
        exec,
        admission,
        host_fingerprint,
        host_key_file,
        dir: Some(dir),
        task,
    }
}

// ------------------------------------------------------------------------------ the client

/// Accepts any host key and remembers its fingerprint.
pub struct TestClient {
    pub host: Arc<Mutex<Option<SshFingerprint>>>,
}

impl client::Handler for TestClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        *self.host.lock().unwrap() = key
            .public_key()
            .to_bytes()
            .ok()
            .map(|b| SshFingerprint::of_blob(&b));
        Ok(true)
    }
}

pub struct Conn {
    pub handle: client::Handle<TestClient>,
    pub host: Arc<Mutex<Option<SshFingerprint>>>,
}

pub async fn connect(addr: SocketAddr) -> Conn {
    let host = Arc::new(Mutex::new(None));
    let handle = within(
        10,
        client::connect(
            Arc::new(client::Config::default()),
            addr,
            TestClient { host: host.clone() },
        ),
    )
    .await
    .expect("connect");
    Conn { handle, host }
}

impl Conn {
    pub async fn login(
        &mut self,
        user: &str,
        key_pem: &str,
        hash: Option<HashAlg>,
    ) -> client::AuthResult {
        let key = PrivateKeyWithHashAlg::new(private(key_pem), hash);
        within(10, self.handle.authenticate_publickey(user, key))
            .await
            .expect("auth round trip")
    }

    /// A session channel, or the reason the server refused to open one.
    pub async fn session(&mut self) -> Result<Channel<client::Msg>, russh::Error> {
        within(10, self.handle.channel_open_session()).await
    }
}

/// What came back on a channel until it closed.
#[derive(Default, Debug)]
pub struct Reply {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub exit: Option<u32>,
    /// `Some(true)` for `Success`, `Some(false)` for `Failure` (the first reply to a request).
    pub accepted: Option<bool>,
    pub closed: bool,
}

/// Reads a channel to its end (or 15 s).
pub async fn drain(ch: &mut Channel<client::Msg>) -> Reply {
    let mut r = Reply::default();
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => r.stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => {
                    r.stderr.push_str(&String::from_utf8_lossy(&data));
                }
                ChannelMsg::ExitStatus { exit_status } => r.exit = Some(exit_status),
                ChannelMsg::Success => {
                    r.accepted.get_or_insert(true);
                }
                ChannelMsg::Failure => {
                    r.accepted.get_or_insert(false);
                }
                ChannelMsg::Close => {
                    r.closed = true;
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    // The server closed its side; a client answers with its own close (OpenSSH does so by
    // itself), which is what frees the server's channel slot.
    if r.closed {
        let _ = ch.close().await;
    }
    r
}

/// The first reply (`Success` or `Failure`) to a request, ignoring everything else.
pub async fn first_reply(ch: &mut Channel<client::Msg>) -> Option<bool> {
    within(10, async {
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Success => return Some(true),
                ChannelMsg::Failure => return Some(false),
                ChannelMsg::Close => return None,
                _ => {}
            }
        }
        None
    })
    .await
}

/// Runs one command on a fresh channel and reads it to its end.
pub async fn run(conn: &mut Conn, command: &[u8]) -> Reply {
    let mut ch = conn.session().await.expect("session");
    ch.exec(true, command.to_vec()).await.unwrap();
    drain(&mut ch).await
}
