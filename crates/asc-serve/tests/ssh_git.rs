//! U87: git over SSH (SPEC §25.11.5; PLAN U87), with real git >= 2.40 as the client and a
//! `russh` client program (`support/ssh_shim.rs`) as its `GIT_SSH_COMMAND`.
//!
//! The server is the production wiring: `GitViewService`, `git_http::register_with_hook` (whose
//! services `GitSshExec` takes), the SSH listener and, beside it, the smart-HTTP routes so the
//! two transports can be compared on one repository. As in `git_push.rs`, the `pre-receive`
//! script runs this test binary's `asc_hook_entry` test.
#![cfg(feature = "git-ssh")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ssh_harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "support/ssh_keys.rs"]
mod keys;
#[path = "support/ssh_shim.rs"]
mod shim;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_git::realgit::RealGit;
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Decision, Principal, SshFingerprint, SshIdentity,
    SshKey, SshKeyKind,
};
use asc_rpc::server::{RepoName, ServerInfo};
use asc_serve::config::{ServeConfig, SshConfig};
use asc_serve::git_push::HookProgram;
use asc_serve::git_view::GitViewService;
use asc_serve::http::Metrics;
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::registry::{Extensions, NewRepo, Registry, RepoEntry, RepoMode};
use asc_serve::ssh::exec::{BUSY, NOT_FOUND, READ_ONLY_KEY, READ_ONLY_MIRROR};
use asc_serve::ssh::{Audit, GitAuditSink, GitRecord, GitSshExec, SshDeps, SshMetrics, SshServer};
use asc_serve::update::{HookRejected, HookRequest as PipelineRequest, PushHooks};
use asc_serve::{git_http, migrate};
use asc_store::refs::Refs;
use async_trait::async_trait;
use harness::{Conn, Records, connect, drain, within};
use keys::*;
use sqlx::Row;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

const WEB: &str = "acme/web";
const LIB: &str = "acme/lib";
const SOLO: &str = "acme/solo";
const MIRROR: &str = "acme/mirror";
const HOOK_TOKEN_VAR: &str = "ASC_GIT_HOOK_TOKEN";

/// alice (user 42) and bob (user 43) have user keys; `DEPLOY_RO` and `DEPLOY_RW` are deploy keys
/// bound to `acme/web`; `STRANGER` is a key nobody registered.
type Key = (&'static str, &'static str, &'static str);
const ALICE: Key = ("alice", ED_A_PRIVATE, ED_A_PUBLIC);
const BOB: Key = ("bob", ED_B_PRIVATE, ED_B_PUBLIC);
const DEPLOY_RO: Key = ("ro", ED_C_PRIVATE, ED_C_PUBLIC);
const DEPLOY_RW: Key = ("rw", EC_PRIVATE, EC_PUBLIC);
const STRANGER: Key = ("stranger", RSA2048_PRIVATE, RSA2048_PUBLIC);

// ------------------------------------------------------------------------------ the hook process

/// The `pre-receive` script runs this test (`--exact asc_hook_entry`) in a fresh process. Under
/// `cargo test` the variable is absent and the test passes at once.
#[test]
fn asc_hook_entry() {
    if std::env::var_os(HOOK_TOKEN_VAR).is_none() {
        return;
    }
    if let Some(q) = std::env::var_os("GIT_QUARANTINE_PATH") {
        let q = PathBuf::from(q);
        if let Some(view) = q.parent().and_then(Path::parent) {
            let mut dump = String::new();
            for (k, v) in std::env::vars_os() {
                dump.push_str(&format!(
                    "env {}={}\n",
                    k.to_string_lossy(),
                    v.to_string_lossy()
                ));
            }
            for a in std::env::args_os() {
                dump.push_str(&format!("arg {}\n", a.to_string_lossy()));
            }
            let _ = std::fs::write(view.join("hook-dump.txt"), dump);
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    std::process::exit(rt.block_on(asc_serve::git_push::run_hook()));
}

/// The `GIT_SSH_COMMAND` program: outside a shim run it passes at once.
#[test]
fn asc_ssh_shim_entry() {
    shim::entry();
}

fn hook_program() -> HookProgram {
    HookProgram {
        exe: std::env::current_exe().unwrap(),
        args: [
            "--exact",
            "asc_hook_entry",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(str::to_owned)
        .to_vec(),
    }
}

// ------------------------------------------------------------------------------ authorizer

/// What a principal may do, by repository.
fn grants(principal: &str) -> &'static [(&'static str, Access)] {
    match principal {
        "42:alice" => &[
            (WEB, Access::Write),
            (LIB, Access::Write),
            (SOLO, Access::Write),
            (MIRROR, Access::Write),
        ],
        "43:bob" => &[(LIB, Access::Write), (SOLO, Access::Read)],
        "deploy-ro" => &[(WEB, Access::Read)],
        "deploy-rw" => &[(WEB, Access::Write)],
        _ => &[],
    }
}

const ALICE_TOKEN: &str = "tok-alice-S3CR3T";

struct Acl {
    keys: HashMap<SshFingerprint, SshIdentity>,
}

impl Acl {
    fn new() -> Self {
        let mut keys = HashMap::new();
        let mut add = |key: Key, kind, principal: &str, repo: Option<&str>| {
            keys.insert(
                harness::fingerprint(key.2),
                SshIdentity {
                    key_id: format!("k-{}", key.0),
                    kind,
                    principal: Principal {
                        id: principal.into(),
                    },
                    repo: repo.map(str::to_owned),
                },
            );
        };
        add(ALICE, SshKeyKind::User, "42:alice", None);
        add(BOB, SshKeyKind::User, "43:bob", None);
        add(DEPLOY_RO, SshKeyKind::Deploy, "deploy-ro", Some(WEB));
        add(DEPLOY_RW, SshKeyKind::Deploy, "deploy-rw", Some(WEB));
        Self { keys }
    }
}

#[async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let Some(token) = token else {
            return Err(AuthError::Unauthenticated);
        };
        if token.expose() != ALICE_TOKEN {
            return Err(AuthError::Unauthenticated);
        }
        if grants("42:alice")
            .iter()
            .any(|(r, max)| *r == repo && access <= *max)
        {
            Ok(Principal {
                id: "42:alice".into(),
            })
        } else {
            Err(AuthError::Forbidden)
        }
    }

    async fn identify_ssh_key(&self, key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        Ok(self.keys.get(&key.fingerprint()).cloned())
    }

    async fn authorize_ssh_key(
        &self,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<Decision, AuthError> {
        let Some(id) = self.keys.get(&key.fingerprint()) else {
            return Ok(Decision::Denied);
        };
        let bound = id.repo.as_deref().is_none_or(|r| r == repo);
        let allowed = grants(&id.principal.id)
            .iter()
            .any(|(r, max)| *r == repo && access <= *max);
        Ok(if bound && allowed {
            Decision::Allowed(id.principal.clone())
        } else {
            Decision::Denied
        })
    }
}

// ------------------------------------------------------------------------------ pipeline hooks

/// Stands for Forgejo's pre-receive: records every call and can refuse.
#[derive(Default)]
struct Recorder {
    pre: Mutex<Vec<(String, Vec<String>)>>,
    refuse: Mutex<Option<String>>,
    /// SQL run on the database while the hook runs: after the push was classified and imported,
    /// before asc's reply to git.
    mutate: Mutex<Option<(sqlx::AnyPool, &'static str)>>,
}

#[async_trait]
impl PushHooks for Recorder {
    async fn pre_receive(&self, req: &PipelineRequest<'_>) -> Result<(), HookRejected> {
        self.pre.lock().unwrap().push((
            req.principal.id.clone(),
            req.updates.iter().map(|u| u.name.clone()).collect(),
        ));
        let mutate = self.mutate.lock().unwrap().clone();
        if let Some((pool, sql)) = mutate {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        match self.refuse.lock().unwrap().clone() {
            Some(m) => Err(HookRejected(m)),
            None => Ok(()),
        }
    }
}

#[derive(Default)]
struct GitRecords(Mutex<Vec<GitRecord>>);

impl GitAuditSink for GitRecords {
    fn record(&self, record: &GitRecord) {
        self.0.lock().unwrap().push(record.clone());
    }
}

// ------------------------------------------------------------------------------ the server

struct Env {
    registry: Arc<Registry>,
    web: Arc<RepoEntry>,
    solo: Arc<RepoEntry>,
    ssh: SocketAddr,
    http: SocketAddr,
    views: Arc<GitViewService>,
    recorder: Arc<Recorder>,
    audit: Arc<GitRecords>,
    dir: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

impl Env {
    /// `limits` are lines of `[limits.git]`; `shared` are lines of `[limits]`.
    async fn start(limits: &str, shared: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let db = format!("sqlite:{}?mode=rwc", fwd(&dir.path().join("asc.db")));
        let cfg = ServeConfig::parse(&format!(
            "store = \"memory:///\"\ndb = {db:?}\ncache = {{ disk_dir = {:?}, disk = \"1GiB\" }}\n\
             [limits]\n{shared}\n[limits.git]\nmin_free_disk = \"1KiB\"\n{limits}\n",
            fwd(&dir.path().join("cache"))
        ))
        .unwrap();
        let pool = migrate::connect(&db, 4).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
        let create = |name: &'static str, mode| {
            let registry = registry.clone();
            async move {
                registry
                    .create(
                        &RepoName::parse(name).unwrap(),
                        &NewRepo {
                            mode,
                            default_branch: "master".into(),
                        },
                    )
                    .await
                    .unwrap()
            }
        };
        let web = create(WEB, RepoMode::Primary).await;
        create(LIB, RepoMode::Primary).await;
        let solo = create(SOLO, RepoMode::Primary).await;
        create(MIRROR, RepoMode::Mirror).await;
        // A branchless repository stores a plain push to a non-shared branch as a user ref.
        sqlx::query("UPDATE repos SET branchless = $1 WHERE id = $2")
            .bind(true)
            .bind(solo.key.0)
            .execute(registry.pool())
            .await
            .unwrap();

        let metrics = Arc::new(Metrics::new());
        let ext = Extensions::new();
        let recorder = Arc::new(Recorder::default());
        assert!(ext.pipeline.set_push_hooks(recorder.clone()).is_ok());
        let views = GitViewService::start(&cfg, pool.clone(), None)
            .unwrap()
            .expect("the git layer is enabled");
        let services = git_http::register_with_hook(
            &ext,
            views.clone(),
            registry.clone(),
            &metrics,
            &cfg,
            &hook_program(),
        );
        let admission = Arc::new(Admission::new(&cfg.limits));
        let acl = Arc::new(Acl::new());

        // Smart HTTP, for the comparison of the two transports.
        let routes = guarded_routes(&ext, acl.clone(), admission.clone(), ServerInfo::default());
        let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let http = incoming.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = Server::builder()
                .accept_http1(true)
                .add_routes(routes)
                .serve_with_incoming(incoming)
                .await;
        });

        // SSH.
        let audit = Arc::new(GitRecords::default());
        let exec = GitSshExec::new(services, admission.clone()).with_audit(audit.clone());
        let ssh_cfg = SshConfig {
            enabled: true,
            listen: "127.0.0.1:0".parse().unwrap(),
            host_key_files: vec![dir.path().join("ssh_host_ed25519_key")],
            ..SshConfig::default()
        };
        let server = SshServer::bind(
            &ssh_cfg,
            &harness::test_limits(),
            SshDeps {
                authorizer: acl,
                admission: Some(admission),
                audit: Audit::new(Arc::new(Records::default()), SshMetrics::detached()),
                exec: Arc::new(exec),
            },
        )
        .await
        .expect("bind");
        let ssh = server.local_addr().unwrap();
        let task = tokio::spawn(server.run(std::future::pending()));
        Self {
            registry,
            web,
            solo,
            ssh,
            http,
            views,
            recorder,
            audit,
            dir,
            _server: task,
        }
    }

    fn url(&self, name: &str) -> String {
        format!("ssh://git@{}/{name}.git", self.ssh)
    }

    fn http_url(&self, name: &str) -> String {
        format!(
            "http://x-access-token:{ALICE_TOKEN}@{}/{name}.git",
            self.http
        )
    }

    /// A git client that logs in with `key`.
    fn client(&self, key: Key) -> Git {
        Git::new(self.dir.path(), key)
    }

    fn view_path(&self, repo: &RepoEntry) -> PathBuf {
        self.views.view_dir().join(format!("{}.git", repo.key.0))
    }

    async fn ref_target(&self, repo: &RepoEntry, name: &str) -> Option<RefTarget> {
        self.registry
            .refs()
            .get(repo.key, &RefName::new(name.as_bytes().to_vec()).unwrap())
            .await
            .unwrap()
            .map(|e| e.target)
    }

    /// The git id of the commit a branch points at.
    async fn git_of(&self, repo: &RepoEntry, name: &str) -> Option<String> {
        match self.ref_target(repo, name).await? {
            RefTarget::Changeset(c) => Some(
                repo.backend
                    .engine()
                    .git_commit_of(c)
                    .await
                    .unwrap()
                    .to_string(),
            ),
            RefTarget::Tag(_) => None,
        }
    }

    async fn ref_names(&self, repo: &RepoEntry) -> Vec<String> {
        self.registry
            .refs()
            .list(repo.key, "refs/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| String::from_utf8_lossy(e.name.as_bytes()).into_owned())
            .collect()
    }

    async fn ref_log(&self, repo: &RepoEntry) -> Vec<(String, String, String)> {
        sqlx::query("SELECT name, via, actor FROM ref_log WHERE repo_id = $1 ORDER BY seq")
            .bind(repo.key.0)
            .fetch_all(self.registry.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.try_get::<String, _>("name").unwrap(),
                    r.try_get::<String, _>("via").unwrap(),
                    r.try_get::<String, _>("actor").unwrap(),
                )
            })
            .collect()
    }

    /// The audit records, once at least `n` exist.
    async fn records(&self, n: usize) -> Vec<GitRecord> {
        for _ in 0..200 {
            let all = self.audit.0.lock().unwrap().clone();
            if all.len() >= n {
                return all;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "fewer than {n} audit records: {:?}",
            self.audit.0.lock().unwrap()
        );
    }
}

// ------------------------------------------------------------------------------ real git

struct Run {
    ok: bool,
    stderr: String,
    stdout: String,
}

struct Git {
    real: RealGit,
    home: tempfile::TempDir,
    ssh_command: String,
}

impl Git {
    fn new(shims: &Path, key: Key) -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("gitconfig"),
            "[user]\n\tname = T\n\temail = t@x\n[init]\n\tdefaultBranch = master\n\
             [core]\n\tautocrlf = false\n[advice]\n\tdetachedHead = false\n",
        )
        .unwrap();
        let shim = shim::Shim::new(shims, key.0, key.1);
        Self {
            real: RealGit::resolve(None, None).expect("real git >= 2.40"),
            home,
            ssh_command: shim.command,
        }
    }

    fn work(&self, name: &str) -> PathBuf {
        let p = self.home.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn run(&self, cwd: &Path, args: &[&str]) -> Run {
        let mut cmd = self.real.command().unwrap();
        cmd.args(["-c", "credential.helper="])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.path().join("gitconfig"))
            .env("HOME", self.home.path())
            .env("GIT_SSH_COMMAND", &self.ssh_command)
            .env("GIT_SSH_VARIANT", "ssh")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = tokio::task::block_in_place(|| cmd.output()).unwrap();
        Run {
            ok: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }

    async fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let r = self.run(cwd, args).await;
        assert!(r.ok, "git {args:?}: {}", r.stderr);
        r.stdout.trim().to_owned()
    }

    async fn repo(&self, name: &str, url: &str) -> PathBuf {
        let dir = self.work(name);
        self.ok(&dir, &["init", "-q"]).await;
        self.ok(&dir, &["remote", "add", "origin", url]).await;
        dir
    }

    async fn commit(&self, dir: &Path, file: &str, content: &str, message: &str) -> String {
        let path = dir.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
        self.ok(dir, &["add", "--all"]).await;
        self.ok(dir, &["commit", "-q", "-m", message]).await;
        self.ok(dir, &["rev-parse", "HEAD"]).await
    }
}

fn remote_says(stderr: &str, text: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.starts_with("remote:") && l.contains(text))
}

/// Deterministic incompressible bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            u8::try_from(x & 0xff).unwrap()
        })
        .collect()
}

/// `ls-remote` output as sorted `(sha, ref)` pairs.
fn refs_of(stdout: &str) -> Vec<(String, String)> {
    let mut v: Vec<_> = stdout
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(s, r)| (s.to_owned(), r.to_owned()))
        .collect();
    v.sort();
    v
}

// ------------------------------------------------------------------------------ raw channels

async fn login(env: &Env, key: Key) -> Conn {
    let mut c = connect(env.ssh).await;
    let r = c.login("git", key.1, None).await;
    assert!(r.success(), "login");
    c
}

/// Starts a pack command on a fresh channel of `conn`.
async fn start_cmd(
    conn: &mut Conn,
    command: &str,
    protocol: Option<&str>,
) -> russh::Channel<russh::client::Msg> {
    let ch = conn.session().await.expect("session");
    if let Some(p) = protocol {
        ch.set_env(false, "GIT_PROTOCOL", p).await.unwrap();
    }
    ch.exec(true, command.as_bytes().to_vec()).await.unwrap();
    ch
}

/// The first data the command sends, or what it said instead.
async fn first_data(
    ch: &mut russh::Channel<russh::client::Msg>,
) -> Result<Vec<u8>, harness::Reply> {
    within(20, async {
        let mut stderr = String::new();
        let mut exit = None;
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => return Ok(data.to_vec()),
                russh::ChannelMsg::ExtendedData { data, .. } => {
                    stderr.push_str(&String::from_utf8_lossy(&data));
                }
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        Err(harness::Reply {
            stderr,
            exit,
            ..harness::Reply::default()
        })
    })
    .await
}

// ------------------------------------------------------------------------------ tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_clone_and_fetch_over_ssh_equal_the_http_view() {
    let env = Env::start("", "").await;
    let git = env.client(ALICE);
    let a = git.repo("a", &env.url(WEB)).await;
    let c1 = git.commit(&a, "README", "one\n", "one").await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(r.ok, "push: {}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c1.clone())
    );
    let c2 = git.commit(&a, "src/lib.txt", "two\n", "two").await;
    git.ok(&a, &["tag", "-a", "-m", "v1", "v1"]).await;
    git.ok(&a, &["tag", "light"]).await;
    let r = git
        .run(&a, &["push", "origin", "master", "v1", "light"])
        .await;
    assert!(r.ok, "push 2: {}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c2.clone())
    );

    // clone, fetch and ls-remote in protocol v0, v1 and v2; the SHAs are the HTTP ones.
    for v in ["0", "1", "2"] {
        let http = git
            .run(&git.work("h"), &["ls-remote", &env.http_url(WEB)])
            .await;
        assert!(http.ok, "{}", http.stderr);
        let want = refs_of(&http.stdout);
        let head = git.ok(&a, &["rev-parse", "HEAD"]).await;
        assert!(
            want.iter()
                .any(|(s, r)| r == "refs/heads/master" && *s == head)
        );
        let proto = format!("protocol.version={v}");
        let ls = git
            .run(&git.work("l"), &["-c", &proto, "ls-remote", &env.url(WEB)])
            .await;
        assert!(ls.ok, "ls-remote v{v}: {}", ls.stderr);
        assert_eq!(refs_of(&ls.stdout), want, "v{v}");
        let work = git.work(&format!("clone{v}"));
        let r = git
            .run(&work, &["-c", &proto, "clone", "-q", &env.url(WEB), "c"])
            .await;
        assert!(r.ok, "clone v{v}: {}", r.stderr);
        let c = work.join("c");
        assert_eq!(git.ok(&c, &["rev-parse", "HEAD"]).await, head);
        assert_eq!(
            std::fs::read_to_string(c.join("src").join("lib.txt")).unwrap(),
            "two\n"
        );
        assert!(git.run(&c, &["fsck", "--strict"]).await.ok);
        // A fetch after a push from the first clone.
        let more = git.commit(&a, "f", &format!("v{v}\n"), "more").await;
        assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
        let r = git.run(&c, &["-c", &proto, "fetch", "-q", "origin"]).await;
        assert!(r.ok, "fetch v{v}: {}", r.stderr);
        assert_eq!(git.ok(&c, &["rev-parse", "origin/master"]).await, more);
    }

    // --depth and --filter.
    let work = git.work("shallow");
    let r = git
        .run(&work, &["clone", "-q", "--depth", "1", &env.url(WEB), "s"])
        .await;
    assert!(r.ok, "shallow clone: {}", r.stderr);
    let count = git
        .ok(&work.join("s"), &["rev-list", "--count", "HEAD"])
        .await;
    assert_eq!(count, "1");
    let work = git.work("partial");
    let r = git
        .run(
            &work,
            &[
                "clone",
                "-q",
                "--filter=blob:none",
                "--no-checkout",
                &env.url(WEB),
                "p",
            ],
        )
        .await;
    assert!(r.ok, "partial clone: {}", r.stderr);

    // The audit records name the transport and the fingerprint, and hold no key blob.
    let records = env.records(4).await;
    let fp = harness::fingerprint(ALICE.2);
    assert!(
        records
            .iter()
            .all(|r| r.transport == "ssh" && r.fingerprint == fp)
    );
    assert!(
        records
            .iter()
            .any(|r| r.service == "git-receive-pack" && r.outcome == "ok")
    );
    assert!(
        records
            .iter()
            .any(|r| r.service == "git-upload-pack" && r.version == "v2")
    );
    let text = format!("{records:?}");
    assert!(!text.contains(ED_A_PUBLIC.split_whitespace().nth(1).unwrap()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_deletes_atomic_pushes_and_the_hook_canary() {
    let env = Env::start("", "").await;
    let git = env.client(ALICE);
    let a = git.repo("a", &env.url(WEB)).await;
    git.commit(&a, "f", "one\n", "one").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);

    // A new shared branch, then its deletion.
    let r = git
        .run(&a, &["push", "origin", "HEAD:refs/heads/agent/x"])
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert!(env.git_of(&env.web, "refs/heads/agent/x").await.is_some());
    let r = git
        .run(&a, &["push", "origin", ":refs/heads/agent/x"])
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert!(env.git_of(&env.web, "refs/heads/agent/x").await.is_none());

    // An atomic push of two good updates, then one with a rewrite of master changes nothing.
    let head = git.ok(&a, &["rev-parse", "HEAD"]).await;
    git.commit(&a, "f", "two\n", "two").await;
    let r = git
        .run(
            &a,
            &[
                "push",
                "--atomic",
                "origin",
                "master",
                "HEAD~1:refs/heads/agent/y",
            ],
        )
        .await;
    assert!(r.ok, "atomic good push: {}", r.stderr);
    git.ok(&a, &["reset", "-q", "--hard", &head]).await;
    git.commit(&a, "g", "diverged\n", "other").await;
    let r = git
        .run(
            &a,
            &[
                "push",
                "--atomic",
                "origin",
                "master",
                "HEAD:refs/heads/agent/z",
            ],
        )
        .await;
    assert!(!r.ok, "a rewrite of master must be refused");
    assert!(env.git_of(&env.web, "refs/heads/agent/z").await.is_none());

    let log = env.ref_log(&env.web).await;
    assert!(
        log.iter()
            .all(|(_, via, actor)| via == "git-push" && actor == "42:alice")
    );
    // The hook saw neither a key nor a token.
    let dump = std::fs::read_to_string(env.view_path(&env.web).join("hook-dump.txt")).unwrap();
    let blob = ED_A_PUBLIC.split_whitespace().nth(1).unwrap();
    let body: String = ED_A_PRIVATE
        .lines()
        .filter(|l| !l.starts_with("---"))
        .collect();
    for needle in [
        blob,
        body.trim(),
        "S3CR3T",
        "SSH_AUTH_SOCK",
        "GIT_SSH_COMMAND",
    ] {
        assert!(
            !needle.is_empty() && !dump.contains(needle),
            "{needle} reached the hook"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_reach_only_what_they_may() {
    let env = Env::start("", "").await;
    let alice = env.client(ALICE);
    let a = alice.repo("a", &env.url(WEB)).await;
    alice.commit(&a, "f", "one\n", "one").await;
    assert!(alice.run(&a, &["push", "origin", "master"]).await.ok);

    // bob has no access to acme/web: the same text as for a repository that does not exist.
    let bob = env.client(BOB);
    let denied = bob.run(&bob.work("x"), &["ls-remote", &env.url(WEB)]).await;
    let missing = bob
        .run(&bob.work("y"), &["ls-remote", &env.url("acme/nothing")])
        .await;
    assert!(!denied.ok && !missing.ok);
    assert!(
        denied.stderr.contains(NOT_FOUND.trim()),
        "{}",
        denied.stderr
    );
    assert_eq!(
        denied.stderr.replace("acme/web", "R"),
        missing.stderr.replace("acme/nothing", "R")
    );
    // ... and a user key reaches its user's repositories.
    let r = bob.run(&bob.work("z"), &["ls-remote", &env.url(LIB)]).await;
    assert!(r.ok, "{}", r.stderr);

    // An unknown key is refused at login.
    let stranger = env.client(STRANGER);
    let r = stranger
        .run(&stranger.work("s"), &["ls-remote", &env.url(WEB)])
        .await;
    assert!(!r.ok);
    assert!(
        r.stderr.contains("Permission denied (publickey)"),
        "{}",
        r.stderr
    );

    // A read deploy key clones; its push is refused. A write deploy key pushes.
    let ro = env.client(DEPLOY_RO);
    let work = ro.work("w");
    let r = ro.run(&work, &["clone", "-q", &env.url(WEB), "c"]).await;
    assert!(r.ok, "{}", r.stderr);
    let c = work.join("c");
    ro.commit(&c, "x", "x\n", "x").await;
    let r = ro
        .run(&c, &["push", "origin", "HEAD:refs/heads/agent/ro"])
        .await;
    assert!(!r.ok);
    assert!(r.stderr.contains(READ_ONLY_KEY.trim()), "{}", r.stderr);
    assert!(env.git_of(&env.web, "refs/heads/agent/ro").await.is_none());
    // A deploy key is bound to its repository.
    let r = ro.run(&c, &["ls-remote", &env.url(LIB)]).await;
    assert!(!r.ok && r.stderr.contains(NOT_FOUND.trim()), "{}", r.stderr);

    let rw = env.client(DEPLOY_RW);
    let b = rw.work("b");
    let r = rw.run(&b, &["clone", "-q", &env.url(WEB), "c"]).await;
    assert!(r.ok, "{}", r.stderr);
    rw.commit(&b.join("c"), "y", "y\n", "y").await;
    let r = rw
        .run(
            &b.join("c"),
            &["push", "origin", "HEAD:refs/heads/agent/rw"],
        )
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert!(env.git_of(&env.web, "refs/heads/agent/rw").await.is_some());
    let log = env.ref_log(&env.web).await;
    assert!(
        log.iter()
            .any(|(n, _, actor)| n == "refs/heads/agent/rw" && actor == "deploy-rw")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_refs_the_pre_receive_path_and_a_mirror() {
    let env = Env::start("", "").await;
    let alice = env.client(ALICE);
    let a = alice.repo("a", &env.url(SOLO)).await;
    alice.commit(&a, "f", "one\n", "one").await;
    assert!(alice.run(&a, &["push", "origin", "master"]).await.ok);

    // A plain push to a non-shared branch: the user-ref line, and a ref only alice has.
    let secret = alice.commit(&a, "secret", "private\n", "private").await;
    let r = alice
        .run(&a, &["push", "origin", "HEAD:refs/heads/feat"])
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert!(
        remote_says(&r.stderr, "asc: stored as your branch 'feat'"),
        "{}",
        r.stderr
    );
    let names = env.ref_names(&env.solo).await;
    assert!(
        names.iter().any(|n| n.contains("refs/users/42/heads/feat")),
        "{names:?}"
    );
    // The user-ref path skips the pipeline's pre-receive: only the master push reached it.
    assert_eq!(env.recorder.pre.lock().unwrap().len(), 1);

    // Hidden refs never appear, in any protocol version, for any reader.
    let bob = env.client(BOB);
    for v in ["0", "1", "2"] {
        let proto = format!("protocol.version={v}");
        let r = bob
            .run(&bob.work("l"), &["-c", &proto, "ls-remote", &env.url(SOLO)])
            .await;
        assert!(r.ok, "{}", r.stderr);
        assert!(
            !r.stdout.contains("refs/users") && !r.stdout.contains("feat"),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains(&secret));
    }
    // The view holds objects no shared ref reaches, so protocol v2 is not served: bob cannot
    // `want` alice's private commit by its id.
    let work = bob.work("fetch");
    bob.ok(&work, &["init", "-q"]).await;
    bob.ok(&work, &["remote", "add", "origin", &env.url(SOLO)])
        .await;
    let r = bob
        .run(
            &work,
            &["-c", "protocol.version=2", "fetch", "-q", "origin", &secret],
        )
        .await;
    assert!(!r.ok, "a private commit was fetched by id");
    assert!(!bob.run(&work, &["cat-file", "-e", &secret]).await.ok);

    // Protocol policy on the wire: v2 on a view without unshared objects, v0 on one with.
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(&mut c, "git-upload-pack '/acme/lib.git'", Some("version=2")).await;
    let first = first_data(&mut ch).await.expect("an advertisement");
    assert!(
        String::from_utf8_lossy(&first).contains("version 2"),
        "{first:?}"
    );
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(
        &mut c,
        "git-upload-pack '/acme/solo.git'",
        Some("version=2"),
    )
    .await;
    let first = String::from_utf8_lossy(&first_data(&mut ch).await.unwrap()).into_owned();
    assert!(
        !first.contains("version 2") && first.contains("refs/heads/master"),
        "{first:?}"
    );
    assert!(!first.contains("refs/users"));
    // Receive-pack never speaks v2.
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(
        &mut c,
        "git-receive-pack '/acme/lib.git'",
        Some("version=2"),
    )
    .await;
    let first = String::from_utf8_lossy(&first_data(&mut ch).await.unwrap()).into_owned();
    assert!(!first.contains("version 2"), "{first:?}");
    drop(ch);

    // Branch protection: the pipeline's reason reaches the client and nothing moves.
    *env.recorder.refuse.lock().unwrap() = Some("asc: master is protected".into());
    alice.commit(&a, "f", "two\n", "two").await;
    let before = env.git_of(&env.solo, "refs/heads/master").await;
    let r = alice.run(&a, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    assert!(
        remote_says(&r.stderr, "asc: master is protected"),
        "{}",
        r.stderr
    );
    assert_eq!(env.git_of(&env.solo, "refs/heads/master").await, before);
    *env.recorder.refuse.lock().unwrap() = None;
    // `-o land` needs the asc client.
    let r = alice
        .run(&a, &["push", "-o", "land", "origin", "master"])
        .await;
    assert!(!r.ok);
    assert!(
        r.stderr.contains("asc: -o land needs the asc client"),
        "{}",
        r.stderr
    );

    // A mirror is read-only even for a writer.
    let m = alice.repo("m", &env.url(MIRROR)).await;
    alice.commit(&m, "f", "m\n", "m").await;
    let r = alice.run(&m, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    assert!(r.stderr.contains(READ_ONLY_MIRROR.trim()), "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fifth_concurrent_fetch_and_the_third_push_are_refused() {
    let env = Env::start("", "").await;
    let mut held = Vec::new();
    // Upload-pack in v0 advertises, then waits for the client: the slot stays taken.
    for _ in 0..4 {
        let mut c = login(&env, ALICE).await;
        let mut ch = start_cmd(&mut c, "git-upload-pack '/acme/web.git'", None).await;
        first_data(&mut ch).await.expect("advertised");
        held.push((c, ch));
    }
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(&mut c, "git-upload-pack '/acme/web.git'", None).await;
    let refused = first_data(&mut ch).await.expect_err("the fifth fetch");
    assert_eq!(refused.exit, Some(1));
    assert!(refused.stderr.contains(BUSY.trim()), "{}", refused.stderr);
    // Another principal is not held back.
    let mut other = login(&env, BOB).await;
    let mut ch = start_cmd(&mut other, "git-upload-pack '/acme/lib.git'", None).await;
    first_data(&mut ch).await.expect("bob fetches");
    drop(held);

    // Pushes: two slots per principal.
    let mut pushes = Vec::new();
    for _ in 0..2 {
        let mut c = login(&env, ALICE).await;
        let mut ch = start_cmd(&mut c, "git-receive-pack '/acme/web.git'", None).await;
        first_data(&mut ch).await.expect("advertised");
        pushes.push((c, ch));
    }
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(&mut c, "git-receive-pack '/acme/web.git'", None).await;
    let refused = first_data(&mut ch).await.expect_err("the third push");
    assert!(refused.stderr.contains(BUSY.trim()), "{}", refused.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_client_is_killed_and_a_dead_push_leaves_nothing() {
    let env = Env::start("idle_timeout = 2\n", "").await;
    // A client that connects and says nothing is cut at the idle limit.
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(&mut c, "git-receive-pack '/acme/web.git'", None).await;
    first_data(&mut ch).await.expect("advertised");
    let started = Instant::now();
    let reply = drain(&mut ch).await;
    assert_eq!(reply.exit, Some(1), "{reply:?}");
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
    let records = env.records(1).await;
    assert!(records.iter().any(|r| r.outcome == "killed"), "{records:?}");

    // A push cut in the middle of its pack: nothing moved, no quarantine stays behind.
    let git = env.client(ALICE);
    let a = git.repo("a", &env.url(WEB)).await;
    std::fs::write(a.join("noise.bin"), noise(600_000, 3)).unwrap();
    git.ok(&a, &["add", "--all"]).await;
    git.ok(&a, &["commit", "-q", "-m", "noisy"]).await;
    let head = git.ok(&a, &["rev-parse", "HEAD"]).await;
    let pack = {
        use std::io::Write as _;
        let mut cmd = git.real.command().unwrap();
        cmd.current_dir(&a)
            .args(["pack-objects", "--stdout", "--revs", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{head}\n").as_bytes())
            .unwrap();
        child.wait_with_output().unwrap().stdout
    };
    let line = format!(
        "{} {head} refs/heads/master\0 report-status side-band-64k agent=t\n",
        "0".repeat(40)
    );
    let mut body = format!("{:04x}{line}0000", line.len() + 4).into_bytes();
    body.extend_from_slice(&pack[..pack.len() / 2]);
    let mut c = login(&env, ALICE).await;
    let mut ch = start_cmd(&mut c, "git-receive-pack '/acme/web.git'", None).await;
    first_data(&mut ch).await.expect("advertised");
    ch.data_bytes(body).await.unwrap();
    drop(ch);
    c.handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await
        .unwrap();
    drop(c);
    let objects = env.view_path(&env.web).join("objects");
    let quarantines = || -> Vec<String> {
        std::fs::read_dir(&objects)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.contains("incoming-"))
                    .collect()
            })
            .unwrap_or_default()
    };
    let started = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if started.elapsed() > Duration::from_secs(6) && quarantines().is_empty() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "left: {:?}",
            quarantines()
        );
    }
    assert!(env.ref_names(&env.web).await.is_empty());
    assert!(env.ref_log(&env.web).await.is_empty());
    // The slot of the dead push was released.
    let b = git.repo("b", &env.url(WEB)).await;
    git.commit(&b, "small", "ok\n", "small").await;
    let r = git.run(&b, &["push", "origin", "master"]).await;
    assert!(r.ok, "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_are_charged_to_the_objects_budget() {
    // A budget far below the size of the push: the stream is cut and the client is told to retry.
    let env = Env::start("", "objects_bytes_per_sec = \"64KiB\"").await;
    let git = env.client(ALICE);
    let a = git.repo("a", &env.url(WEB)).await;
    std::fs::write(a.join("noise.bin"), noise(200_000, 9)).unwrap();
    git.ok(&a, &["add", "--all"]).await;
    git.ok(&a, &["commit", "-q", "-m", "noisy"]).await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(!r.ok, "{}", r.stderr);
    let records = env.records(1).await;
    assert!(
        records
            .iter()
            .any(|r| r.outcome == "limit:objects_budget" && r.exit_status == 1),
        "{records:?}"
    );
    assert!(env.ref_names(&env.web).await.is_empty());
}

// ------------------------------------------------------------------------------ fail-closed user refs

fn pkt(text: &str) -> Vec<u8> {
    format!("{:04x}{text}", text.len() + 4).into_bytes()
}

fn marker_of(env: &Env) -> PathBuf {
    env.view_path(&env.solo).join("asc-view-unshared")
}

/// A mixed push (`master` and a private branch) whose repository policy changes, or breaks,
/// while the pipeline's hook runs: the private branch is still reported, hidden and marked as
/// the stored user ref it is, because the answer comes from the call that stored it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_policy_that_changes_after_the_import_cannot_unhide_a_user_ref() {
    for change in [
        "UPDATE repos SET shared_refs = 'not json'",
        "UPDATE repos SET shared_refs = '[\"master\",\"feat\"]'",
        "UPDATE repos SET default_branch = 'feat'",
    ] {
        let env = Env::start("", "").await;
        let alice = env.client(ALICE);
        let a = alice.repo("a", &env.url(SOLO)).await;
        alice
            .commit(
                &a, "f", "one
", "one",
            )
            .await;
        assert!(alice.run(&a, &["push", "origin", "master"]).await.ok);
        assert!(!marker_of(&env).exists());

        alice
            .commit(
                &a, "f", "two
", "two",
            )
            .await;
        *env.recorder.mutate.lock().unwrap() = Some((env.registry.pool().clone(), change));
        let r = alice
            .run(&a, &["push", "origin", "master", "HEAD:refs/heads/feat"])
            .await;
        *env.recorder.mutate.lock().unwrap() = None;
        assert!(r.ok, "{change}: {}", r.stderr);
        assert!(
            remote_says(&r.stderr, "asc: stored as your branch 'feat'"),
            "{change}: {}",
            r.stderr
        );
        assert!(marker_of(&env).exists(), "{change}");
        let names = env.ref_names(&env.solo).await;
        assert!(names.iter().any(|n| n.contains("refs/users/42/heads/feat")));
    }
}

/// A policy that cannot be read still marks the view before anything is imported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_policy_marks_the_view_before_the_import() {
    let env = Env::start("", "").await;
    let alice = env.client(ALICE);
    let a = alice.repo("a", &env.url(SOLO)).await;
    alice
        .commit(
            &a, "f", "one
", "one",
        )
        .await;
    assert!(alice.run(&a, &["push", "origin", "master"]).await.ok);
    sqlx::query("UPDATE repos SET shared_refs = 'not json'")
        .execute(env.registry.pool())
        .await
        .unwrap();
    alice
        .commit(
            &a, "f", "two
", "two",
        )
        .await;
    let r = alice
        .run(&a, &["push", "origin", "HEAD:refs/heads/feat"])
        .await;
    assert!(!r.ok, "{}", r.stderr);
    assert!(marker_of(&env).exists());
    let names = env.ref_names(&env.solo).await;
    assert!(!names.iter().any(|n| n.contains("refs/users")), "{names:?}");
}

/// A push that may store a user ref is refused when the unshared marker cannot be written, and
/// nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_marker_write_refuses_the_push() {
    let env = Env::start("", "").await;
    let alice = env.client(ALICE);
    let a = alice.repo("a", &env.url(SOLO)).await;
    alice
        .commit(
            &a, "f", "one
", "one",
        )
        .await;
    assert!(alice.run(&a, &["push", "origin", "master"]).await.ok);
    // A directory where the marker file goes: the write fails.
    std::fs::create_dir(marker_of(&env)).unwrap();
    alice
        .commit(
            &a, "f", "two
", "two",
        )
        .await;
    let r = alice
        .run(&a, &["push", "origin", "HEAD:refs/heads/feat"])
        .await;
    assert!(!r.ok, "{}", r.stderr);
    let names = env.ref_names(&env.solo).await;
    assert!(!names.iter().any(|n| n.contains("refs/users")), "{names:?}");
}

/// A protocol v2 session opened before a private push cannot `want` the private commit after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_v2_session_open_during_a_private_push_cannot_fetch_it() {
    let env = Env::start("", "").await;
    let alice = env.client(ALICE);
    let a = alice.repo("a", &env.url(SOLO)).await;
    alice
        .commit(
            &a, "f", "one
", "one",
        )
        .await;
    assert!(alice.run(&a, &["push", "origin", "master"]).await.ok);

    let mut c = login(&env, BOB).await;
    let mut ch = start_cmd(
        &mut c,
        "git-upload-pack '/acme/solo.git'",
        Some("version=2"),
    )
    .await;
    let first = first_data(&mut ch).await.expect("an advertisement");
    assert!(String::from_utf8_lossy(&first).contains("version 2"));

    let secret = alice
        .commit(
            &a, "secret", "private
", "private",
        )
        .await;
    let r = alice
        .run(&a, &["push", "origin", "HEAD:refs/heads/feat"])
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert!(marker_of(&env).exists());

    let mut request = pkt("command=fetch
");
    request.extend(pkt("object-format=sha1
"));
    request.extend(b"0001");
    request.extend(pkt(&format!(
        "want {secret}
"
    )));
    request.extend(pkt("done
"));
    request.extend(b"0000");
    ch.data(&request[..]).await.unwrap();
    let (data, stderr) = within(20, async {
        let (mut data, mut stderr) = (Vec::new(), String::new());
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { data: d } => data.extend_from_slice(&d),
                russh::ChannelMsg::ExtendedData { data: d, .. } => {
                    stderr.push_str(&String::from_utf8_lossy(&d));
                }
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        (data, stderr)
    })
    .await;
    assert!(
        !data.windows(4).any(|w| w == b"PACK"),
        "a private commit was served over a v2 session: {}",
        String::from_utf8_lossy(&data)
    );
    assert!(stderr.contains("changed"), "{stderr}");
}
