//! U82: `git push` against `asc serve` (SPEC §25.2 "Writes", §25.5, §25.6, §25.8; PLAN U82), with
//! real git >= 2.40 as the client and real `git receive-pack` behind the routes.
//!
//! The server is the production wiring: `GitViewService`, `git_http::register_with_hook`, the
//! credential edge and `limits::guarded_routes`, served by tonic's server over a loopback socket.
//! The only difference is the program the asc-owned `pre-receive` script executes: the `asc` binary
//! does not exist for this crate's tests, so the script runs this test binary's
//! `asc_hook_entry` test, which calls the very function `asc serve git-hook` calls
//! (`git_push::run_hook`). The server side of the hook (socket, token, quarantine, import,
//! `UpdateRefs`) is the real one.
//!
//! Scaled limits: the brief's 2,000,001 objects and 513 MiB blob are tested with the same limits
//! set low (`max_push_objects`, `max_object_bytes`, `max_push_bytes`, `child_memory`), which
//! exercise the same code without building multi-gigabyte inputs.
#![cfg(feature = "git-compat")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_git::compat::hook::{ENV_ENDPOINT, ENV_TOKEN, HookCommand, HookInput, HookRequest};
use asc_git::realgit::RealGit;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::{CallCtx, RepoName, ServerInfo};
use asc_serve::config::ServeConfig;
use asc_serve::git_push::{GitPush, HookProgram, sweep_quarantines};
use asc_serve::git_view::GitViewService;
use asc_serve::http::Metrics;
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::registry::{Extensions, NewRepo, Registry, RepoEntry, RepoMode};
use asc_serve::update::{HookRejected, HookRequest as PipelineRequest, PushHooks};
use asc_serve::{git_http, migrate};
use asc_store::refs::Refs;
use sqlx::Row;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use uuid::Uuid;

const WRITE: &str = "tok-write-S3CR3T";
const READ: &str = "tok-read-S3CR3T";
const OUTSIDE: &str = "tok-outside-S3CR3T";
const OTHER: &str = "tok-other-S3CR3T";
const WEB: &str = "acme/web";
const LIB: &str = "acme/lib";
const MIRROR: &str = "acme/mirror";
/// Set in the environment of the hook process only (by the `pre-receive` script's `receive-pack`).
const HOOK_TOKEN_VAR: &str = "ASC_GIT_HOOK_TOKEN";
/// The text a hook is given for a token that is unknown, used or expired.
const TOKEN_REFUSED: &str = "asc: this push is no longer accepted; push again";

// ------------------------------------------------------------------------------ the hook process

/// The `pre-receive` script runs this test (`--exact asc_hook_entry`) in a fresh process. Under
/// `cargo test` the variable is absent and the test passes at once.
#[test]
fn asc_hook_entry() {
    if std::env::var_os(HOOK_TOKEN_VAR).is_none() {
        return;
    }
    // What the hook can see, for the canary test: next to the view, `hook-dump.txt`.
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
    let code = rt.block_on(asc_serve::git_push::run_hook());
    std::process::exit(code);
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

struct Acl;

impl Acl {
    fn grants(token: &str) -> Option<(&'static str, Vec<(&'static str, Access)>)> {
        match token {
            WRITE => Some((
                "writer",
                vec![
                    (WEB, Access::Write),
                    (LIB, Access::Write),
                    (MIRROR, Access::Write),
                ],
            )),
            OTHER => Some(("other", vec![(WEB, Access::Write)])),
            READ => Some(("reader", vec![(WEB, Access::Read)])),
            OUTSIDE => Some(("outsider", vec![])),
            _ => None,
        }
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
        let (id, repos) = Self::grants(token.expose()).ok_or(AuthError::Unauthenticated)?;
        if repos.iter().any(|(r, max)| *r == repo && access <= *max) {
            Ok(Principal { id: id.to_owned() })
        } else {
            Err(AuthError::Forbidden)
        }
    }
}

// ------------------------------------------------------------------------------ pipeline hooks

/// One call of the pre-receive hook: the principal, the refs, the push options.
type PreCall = (String, Vec<String>, Vec<String>);

/// Stands for Forgejo's pre-receive: records every call, can refuse, and can be held.
#[derive(Default)]
struct Recorder {
    pre: Mutex<Vec<PreCall>>,
    refuse: Mutex<Option<String>>,
    /// Hold this lock to keep a push inside its pre-receive hook.
    block: tokio::sync::Mutex<()>,
}

#[async_trait]
impl PushHooks for Recorder {
    async fn pre_receive(&self, req: &PipelineRequest<'_>) -> Result<(), HookRejected> {
        self.pre.lock().unwrap().push((
            req.principal.id.clone(),
            req.updates.iter().map(|u| u.name.clone()).collect(),
            req.push_options.to_vec(),
        ));
        let _held = self.block.lock().await;
        match self.refuse.lock().unwrap().clone() {
            Some(m) => Err(HookRejected(m)),
            None => Ok(()),
        }
    }
}

// ------------------------------------------------------------------------------ the server

struct Env {
    registry: Arc<Registry>,
    web: Arc<RepoEntry>,
    lib: Arc<RepoEntry>,
    addr: SocketAddr,
    views: Arc<GitViewService>,
    recorder: Arc<Recorder>,
    ext: Extensions,
    _dir: tempfile::TempDir,
}

impl Env {
    /// `limits` are lines of `[limits.git]`; `tables` are further tables.
    async fn start(limits: &str, tables: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let db = format!("sqlite:{}?mode=rwc", fwd(&dir.path().join("asc.db")));
        // A disk gate any machine passes, unless the test sets its own.
        let gate = if limits.contains("min_free_disk") {
            ""
        } else {
            "min_free_disk = \"1KiB\"\n"
        };
        let cfg = ServeConfig::parse(&format!(
            "store = \"memory:///\"\ndb = {db:?}\ncache = {{ disk_dir = {:?}, disk = \"1GiB\" }}\n\
             [limits.git]\n{gate}{limits}\n{tables}\n",
            fwd(&dir.path().join("cache"))
        ))
        .unwrap();
        let pool = migrate::connect(&db, 4).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
        let web = registry
            .create(&RepoName::parse(WEB).unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let lib = registry
            .create(&RepoName::parse(LIB).unwrap(), &NewRepo::default())
            .await
            .unwrap();
        registry
            .create(
                &RepoName::parse(MIRROR).unwrap(),
                &NewRepo {
                    mode: RepoMode::Mirror,
                    default_branch: "trunk".into(),
                },
            )
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let ext = Extensions::new();
        let recorder = Arc::new(Recorder::default());
        assert!(ext.pipeline.set_push_hooks(recorder.clone()).is_ok());
        let views = GitViewService::start(&cfg, pool.clone(), None)
            .unwrap()
            .expect("the git layer is enabled");
        git_http::register_with_hook(
            &ext,
            views.clone(),
            registry.clone(),
            &metrics,
            &cfg,
            &hook_program(),
        );
        let admission = Arc::new(Admission::new(&cfg.limits));
        let routes = guarded_routes(&ext, Arc::new(Acl), admission, ServerInfo::default());
        let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = incoming.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = Server::builder()
                .accept_http1(true)
                .add_routes(routes)
                .serve_with_incoming(incoming)
                .await;
        });
        Self {
            registry,
            web,
            lib,
            addr,
            views,
            recorder,
            ext,
            _dir: dir,
        }
    }

    fn url(&self, name: &str, token: &str) -> String {
        format!("http://x-access-token:{token}@{}/{name}.git", self.addr)
    }

    fn view_path(&self, repo: &RepoEntry) -> PathBuf {
        self.views.view_dir().join(format!("{}.git", repo.key.0))
    }

    async fn ref_entry(&self, repo: &RepoEntry, name: &str) -> Option<asc_store::refs::RefEntry> {
        self.registry
            .refs()
            .get(repo.key, &RefName::new(name.as_bytes().to_vec()).unwrap())
            .await
            .unwrap()
    }

    async fn ref_target(&self, repo: &RepoEntry, name: &str) -> Option<RefTarget> {
        self.ref_entry(repo, name).await.map(|e| e.target)
    }

    /// The git id of the commit a branch or lightweight tag points at.
    async fn git_of(&self, repo: &RepoEntry, name: &str) -> Option<String> {
        match self.ref_target(repo, name).await? {
            RefTarget::Changeset(c) => Some(self.git_commit(repo, c).await),
            RefTarget::Tag(_) => None,
        }
    }

    async fn git_commit(&self, repo: &RepoEntry, c: ChangesetId) -> String {
        repo.backend
            .engine()
            .git_commit_of(c)
            .await
            .unwrap()
            .to_string()
    }

    /// Whether `git_map` knows the git object `sha` in `repo`.
    async fn mapped(&self, repo: &RepoEntry, sha: &str) -> bool {
        let git: Vec<u8> = (0..20)
            .map(|i| u8::from_str_radix(&sha[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        sqlx::query("SELECT COUNT(*) FROM git_map WHERE ns = $1 AND git = $2")
            .bind(repo.ns.as_bytes().to_vec())
            .bind(git)
            .fetch_one(self.registry.pool())
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
            > 0
    }

    /// `ref_log` rows of `repo` as `(name, via, actor)`.
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

    /// Nothing of a refused push may exist: no ref, no git map row, no ref log row.
    async fn assert_empty(&self, repo: &RepoEntry) {
        assert!(
            self.registry
                .refs()
                .list(repo.key, "refs/")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(self.ref_log(repo).await.is_empty());
        let rows = sqlx::query("SELECT COUNT(*) FROM git_map WHERE ns = $1")
            .bind(repo.ns.as_bytes().to_vec())
            .fetch_one(self.registry.pool())
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap();
        assert_eq!(rows, 0, "a refused push left git_map rows");
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
}

impl Git {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("gitconfig"),
            "[user]\n\tname = T\n\temail = t@x\n[init]\n\tdefaultBranch = master\n\
             [core]\n\tautocrlf = false\n[advice]\n\tdetachedHead = false\n",
        )
        .unwrap();
        Self {
            real: RealGit::resolve(None, None).expect("real git >= 2.40"),
            home,
        }
    }

    fn work(&self, name: &str) -> PathBuf {
        let p = self.home.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sync(&self, cwd: &Path, args: &[&str], input: &[u8]) -> Run {
        let mut cmd = self.real.command().unwrap();
        cmd.args(["-c", "credential.helper="])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.path().join("gitconfig"))
            .env("HOME", self.home.path())
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let feeder = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().unwrap();
        let _ = feeder.join();
        Run {
            ok: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }

    async fn run(&self, cwd: &Path, args: &[&str]) -> Run {
        tokio::task::block_in_place(|| self.sync(cwd, args, b""))
    }

    /// `git <args>`, which must succeed; returns its trimmed stdout.
    async fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let r = self.run(cwd, args).await;
        assert!(r.ok, "git {args:?}: {}", r.stderr);
        r.stdout.trim().to_owned()
    }

    /// A new repository with `origin` pointing at `url`.
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

    /// `git hash-object -t <kind> --literally -w --stdin`: writes any bytes as an object.
    async fn object(&self, dir: &Path, kind: &str, bytes: &[u8]) -> String {
        let r = tokio::task::block_in_place(|| {
            self.sync(
                dir,
                &["hash-object", "-t", kind, "--literally", "-w", "--stdin"],
                bytes,
            )
        });
        assert!(r.ok, "hash-object: {}", r.stderr);
        r.stdout.trim().to_owned()
    }

    /// A tree from `(mode, name, blob-or-tree id)` entries, which must already be in git's order.
    async fn tree(&self, dir: &Path, entries: &[(&str, &str, &str)]) -> String {
        let mut raw = Vec::new();
        for (mode, name, id) in entries {
            raw.extend_from_slice(format!("{mode} {name}").as_bytes());
            raw.push(0);
            raw.extend((0..20).map(|i| u8::from_str_radix(&id[2 * i..2 * i + 2], 16).unwrap()));
        }
        self.object(dir, "tree", &raw).await
    }

    async fn commit_tree(&self, dir: &Path, tree: &str, parent: Option<&str>, msg: &str) -> String {
        let mut args = vec!["commit-tree", tree, "-m", msg];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        self.ok(dir, &args).await
    }
}

/// Whether git printed a `remote:` line with `text` (the hook's refusal, relayed by git).
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

// ------------------------------------------------------------------------------ raw http

struct Raw {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn basic(token: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
    )
}

async fn read_response(mut s: tokio::net::TcpStream) -> Raw {
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut buf)).await;
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(buf.len(), |p| p + 4);
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut body = buf[split..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    Raw { status, head, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(p) = b.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(std::str::from_utf8(&b[..p]).unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        if size == 0 || b.len() < p + 2 + size {
            break;
        }
        out.extend_from_slice(&b[p + 2..p + 2 + size]);
        b = &b[(p + 2 + size + 2).min(b.len())..];
    }
    out
}

fn request(
    method: &str,
    target: &str,
    host: SocketAddr,
    auth: Option<&str>,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut r = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(a) = auth {
        r.push_str(&format!("Authorization: {a}\r\n"));
    }
    for (k, v) in extra {
        r.push_str(&format!("{k}: {v}\r\n"));
    }
    if method == "POST" {
        r.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    r.push_str("\r\n");
    let mut bytes = r.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn raw(addr: SocketAddr, req: Vec<u8>) -> Raw {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(&req).await.unwrap();
    read_response(s).await
}

const PUSH_TYPE: (&str, &str) = ("Content-Type", "application/x-git-receive-pack-request");

fn pkt(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

// ------------------------------------------------------------------------------ tests: happy path

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_a_branch_then_fast_forward_it_and_read_it_back() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let url = env.url(WEB, WRITE);
    let a = git.repo("a", &url).await;

    // The first push to an empty repo creates master.
    let c1 = git.commit(&a, "README", "one\n", "one").await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(r.ok, "push: {}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c1.clone())
    );

    // A fast-forward, and the pushed commits map back to the same SHAs for a later reader.
    let c2 = git.commit(&a, "src/lib.txt", "two\n", "two").await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(r.ok, "push 2: {}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c2.clone())
    );

    let b = git.work("b");
    let r = git.run(&b, &["clone", "-q", &url, "c"]).await;
    assert!(r.ok, "clone: {}", r.stderr);
    assert_eq!(git.ok(&b.join("c"), &["rev-parse", "HEAD"]).await, c2);
    assert_eq!(
        std::fs::read_to_string(b.join("c").join("src").join("lib.txt")).unwrap(),
        "two\n"
    );
    assert!(git.run(&b.join("c"), &["fsck", "--strict"]).await.ok);
    // A second clone's push on top of the first one's history joins without a client-side step.
    let c3 = git
        .commit(&b.join("c"), "more.txt", "three\n", "three")
        .await;
    let r = git.run(&b.join("c"), &["push", "origin", "master"]).await;
    assert!(r.ok, "push from the clone: {}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c3.clone())
    );
    // The first client sees it with a plain fetch (the view followed asc's refs).
    git.ok(&a, &["fetch", "-q", "origin"]).await;
    assert_eq!(git.ok(&a, &["rev-parse", "origin/master"]).await, c3);

    // The ref log says who pushed and how; the outbox carries `post_receive`.
    let log = env.ref_log(&env.web).await;
    assert_eq!(log.len(), 3);
    for (name, via, actor) in &log {
        assert_eq!(
            (name.as_str(), via.as_str(), actor.as_str()),
            ("refs/heads/master", "git-push", "writer")
        );
    }
    let outbox: Vec<Vec<u8>> = sqlx::query("SELECT effects FROM outbox WHERE repo_id = $1")
        .bind(env.web.key.0)
        .fetch_all(env.registry.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<Vec<u8>, _>("effects").unwrap())
        .collect();
    assert!(outbox.len() >= 3);
    assert!(
        outbox
            .iter()
            .all(|e| String::from_utf8_lossy(e).contains("post_receive"))
    );
    // The pipeline ran Forgejo's pre-receive hook (it is never skipped for a client).
    let pre = env.recorder.pre.lock().unwrap().clone();
    assert_eq!(pre.len(), 3);
    assert_eq!(pre[0].0, "writer");
    assert_eq!(pre[0].1, ["refs/heads/master"]);
    // The hook's environment held no credential (canary).
    let dump = std::fs::read_to_string(env.view_path(&env.web).join("hook-dump.txt")).unwrap();
    assert!(dump.contains(ENV_TOKEN) && dump.contains(ENV_ENDPOINT));
    for needle in ["S3CR3T", "Authorization", "authorization", "x-access-token"] {
        assert!(!dump.contains(needle), "{needle} reached the hook:\n{dump}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_tags_deletes_and_atomic_pushes() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let url = env.url(WEB, WRITE);
    let a = git.repo("a", &url).await;
    let c1 = git.commit(&a, "README", "one\n", "one").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);

    // A new branch, then its deletion.
    git.ok(&a, &["branch", "feat"]).await;
    let r = git.run(&a, &["push", "origin", "feat"]).await;
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/feat").await,
        Some(c1.clone())
    );
    let r = git.run(&a, &["push", "origin", ":feat"]).await;
    assert!(r.ok, "{}", r.stderr);
    assert!(env.ref_target(&env.web, "refs/heads/feat").await.is_none());
    let ls = git.ok(&a, &["ls-remote", "origin"]).await;
    assert!(!ls.contains("refs/heads/feat"), "{ls}");

    // A lightweight and an annotated tag.
    git.ok(&a, &["tag", "v1"]).await;
    git.ok(&a, &["tag", "-a", "v2", "-m", "release two"]).await;
    let r = git.run(&a, &["push", "origin", "v1", "v2"]).await;
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(env.git_of(&env.web, "refs/tags/v1").await, Some(c1.clone()));
    assert!(matches!(
        env.ref_target(&env.web, "refs/tags/v2").await,
        Some(RefTarget::Tag(_))
    ));
    let ls = git.ok(&a, &["ls-remote", "origin"]).await;
    let v2_tag = git.ok(&a, &["rev-parse", "v2"]).await;
    assert!(ls.contains(&format!("{v2_tag}\trefs/tags/v2")), "{ls}");
    assert!(ls.contains(&format!("{c1}\trefs/tags/v2^{{}}")), "{ls}");

    // `--atomic`: one ref the pipeline cannot take sinks the whole push (the hook is all or
    // nothing), so the good ref does not move either.
    let c2 = git.commit(&a, "README", "two\n", "two").await;
    let r = git
        .run(
            &a,
            &[
                "push",
                "--atomic",
                "origin",
                "master",
                "HEAD:refs/heads/side",
                "HEAD:refs/for/master",
            ],
        )
        .await;
    assert!(!r.ok, "{}", r.stderr);
    assert!(remote_says(&r.stderr, "asc:"), "{}", r.stderr);
    assert!(
        r.stderr.contains("pre-receive hook declined"),
        "{}",
        r.stderr
    );
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c1.clone())
    );
    assert!(env.ref_target(&env.web, "refs/heads/side").await.is_none());
    // Without the bad ref the same push goes through.
    let r = git
        .run(
            &a,
            &[
                "push",
                "--atomic",
                "origin",
                "master",
                "HEAD:refs/heads/side",
            ],
        )
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(c2.clone())
    );
    assert_eq!(env.git_of(&env.web, "refs/heads/side").await, Some(c2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_rewrite_of_a_shared_ref_is_refused() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    git.commit(&a, "f", "one\n", "one").await;
    let c2 = git.commit(&a, "f", "two\n", "two").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    // Rewrite: a sibling of c2 on the first commit.
    git.ok(&a, &["reset", "-q", "--hard", "HEAD~1"]).await;
    git.commit(&a, "g", "other\n", "other").await;
    let r = git.run(&a, &["push", "--force", "origin", "master"]).await;
    assert!(!r.ok);
    assert!(r.stderr.contains("remote rejected"), "{}", r.stderr);
    assert!(r.stderr.contains("non-fast-forward"), "{}", r.stderr);
    assert_eq!(env.git_of(&env.web, "refs/heads/master").await, Some(c2));
}

// ------------------------------------------------------------------------------ tests: races

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_race_is_rejected_and_nothing_of_it_is_indexed() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let url = env.url(WEB, WRITE);
    let a = git.repo("a", &url).await;
    git.commit(&a, "f", "base\n", "base").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);

    // Client B has read master (= base) and is about to push; its pre-push hook parks it there.
    let b = git.repo("b", &url).await;
    git.ok(&b, &["pull", "-q", "origin", "master"]).await;
    let loser = git.commit(&b, "f", "loser\n", "loser").await;
    let sync = git.work("sync");
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let hook = format!(
        "#!/bin/sh\n: > '{0}/ready'\nwhile [ ! -f '{0}/go' ]; do sleep 0.1; done\n",
        fwd(&sync)
    );
    let hook_path = b.join(".git").join("hooks").join("pre-push");
    std::fs::write(&hook_path, hook).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let git = Arc::new(git);
    let pushing = {
        let (git, b) = (git.clone(), b.clone());
        tokio::task::spawn_blocking(move || git.sync(&b, &["push", "origin", "master"], b""))
    };
    let started = Instant::now();
    while !sync.join("ready").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "B never reached its pre-push hook"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A wins the race.
    let winner = git.commit(&a, "f", "winner\n", "winner").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    std::fs::write(sync.join("go"), b"").unwrap();

    let r = pushing.await.unwrap();
    assert!(!r.ok, "{}", r.stderr);
    assert!(r.stderr.contains("remote rejected"), "{}", r.stderr);
    // The view already shows A's push when the hook runs, so the refusal comes before the commit
    // ("changed while you pushed"); a race inside the commit is "was changed by someone else".
    assert!(
        r.stderr.contains("changed while you pushed")
            || r.stderr.contains("was changed by someone else"),
        "the hook's reason is relayed: {}",
        r.stderr
    );
    assert_eq!(
        env.git_of(&env.web, "refs/heads/master").await,
        Some(winner)
    );
    assert!(
        !env.mapped(&env.web, &loser).await,
        "the loser's commit was indexed"
    );
    let log = env.ref_log(&env.web).await;
    assert_eq!(
        log.len(),
        2,
        "only the two successful pushes are in the log"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_push_slots_cap_one_principal_but_not_another() {
    let env = Env::start("push_per_principal = 1", "").await;
    let git = Arc::new(Git::new());
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    git.commit(&a, "f", "one\n", "one").await;

    // Hold the first push inside Forgejo's pre-receive, with its slot taken.
    let held = env.recorder.block.lock().await;
    let first = {
        let (git, a) = (git.clone(), a.clone());
        tokio::task::spawn_blocking(move || git.sync(&a, &["push", "origin", "master"], b""))
    };
    let started = Instant::now();
    while env.recorder.pre.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the push never reached the hook"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Two flushes: not the probe, so it needs a slot.
    let body = b"00000000";
    let target = format!("/{WEB}.git/git-receive-pack");
    let busy = raw(
        env.addr,
        request(
            "POST",
            &target,
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE],
            body,
        ),
    )
    .await;
    assert_eq!(busy.status, 429, "{}", busy.head);
    assert!(busy.header("retry-after").is_some());
    assert!(busy.text().contains("too many pushes"), "{}", busy.text());
    // Another principal has its own slot.
    let free = raw(
        env.addr,
        request(
            "POST",
            &target,
            env.addr,
            Some(&basic(OTHER)),
            &[PUSH_TYPE],
            body,
        ),
    )
    .await;
    assert_eq!(free.status, 200, "{} {}", free.head, free.text());

    drop(held);
    let r = first.await.unwrap();
    assert!(r.ok, "{}", r.stderr);
    // The slot is free again.
    let again = raw(
        env.addr,
        request(
            "POST",
            &target,
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE],
            body,
        ),
    )
    .await;
    assert_eq!(again.status, 200, "{}", again.head);
}

// ------------------------------------------------------------------------------ tests: access

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receive_pack_needs_write_and_a_mirror_takes_no_push() {
    let env = Env::start("", "").await;
    let get = |repo: &'static str, auth: Option<String>| {
        let addr = env.addr;
        async move {
            let target = format!("/{repo}.git/info/refs?service=git-receive-pack");
            raw(
                addr,
                request("GET", &target, addr, auth.as_deref(), &[], b""),
            )
            .await
        }
    };
    // Write access: the advertisement of receive-pack.
    let ok = get(WEB, Some(basic(WRITE))).await;
    assert_eq!(ok.status, 200, "{}", ok.head);
    assert_eq!(
        ok.header("content-type").as_deref(),
        Some("application/x-git-receive-pack-advertisement")
    );
    let body = ok.text();
    assert!(
        body.starts_with("001f# service=git-receive-pack\n0000"),
        "{body:?}"
    );
    assert!(
        body.contains("report-status") && body.contains("push-options"),
        "{body:?}"
    );
    assert!(
        body.contains("atomic") && body.contains("side-band-64k"),
        "{body:?}"
    );
    assert!(
        !body.contains("git/"),
        "the git version must not be advertised: {body:?}"
    );
    // Read-only: 403; no read access: the same 404 as a missing repo; anonymous: the challenge;
    // an unknown credential: 401.
    assert_eq!(get(WEB, Some(basic(READ))).await.status, 403);
    assert_eq!(get(WEB, Some(basic(OUTSIDE))).await.status, 404);
    assert_eq!(get("acme/nothing", Some(basic(WRITE))).await.status, 404);
    let anon = get(WEB, None).await;
    assert_eq!(anon.status, 401);
    assert!(
        anon.header("www-authenticate")
            .unwrap()
            .starts_with("Basic")
    );
    assert_eq!(get(WEB, Some(basic("revoked"))).await.status, 401);
    // A mirror is read-only even for a writer, on the advertisement and on the push.
    let mirror = get(MIRROR, Some(basic(WRITE))).await;
    assert_eq!(mirror.status, 403);
    assert!(
        mirror.text().contains("read-only mirror"),
        "{}",
        mirror.text()
    );
    let post = raw(
        env.addr,
        request(
            "POST",
            &format!("/{MIRROR}.git/git-receive-pack"),
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE],
            b"00000000",
        ),
    )
    .await;
    assert_eq!(post.status, 403);
    // A read token cannot push either.
    let post = raw(
        env.addr,
        request(
            "POST",
            &format!("/{WEB}.git/git-receive-pack"),
            env.addr,
            Some(&basic(READ)),
            &[PUSH_TYPE],
            b"00000000",
        ),
    )
    .await;
    assert_eq!(post.status, 403);
    // The `0000` probe is answered before a child starts; a wrong content type or an encoded
    // body is a 415.
    let target = format!("/{WEB}.git/git-receive-pack");
    let probe = raw(
        env.addr,
        request(
            "POST",
            &target,
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE],
            b"0000",
        ),
    )
    .await;
    assert_eq!(probe.status, 200);
    assert_eq!(
        probe.header("content-type").as_deref(),
        Some("application/x-git-receive-pack-result")
    );
    assert!(probe.body.is_empty());
    let wrong = raw(
        env.addr,
        request(
            "POST",
            &target,
            env.addr,
            Some(&basic(WRITE)),
            &[("Content-Type", "application/x-git-upload-pack-request")],
            b"00000000",
        ),
    )
    .await;
    assert_eq!(wrong.status, 415);
    let gz = raw(
        env.addr,
        request(
            "POST",
            &target,
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE, ("Content-Encoding", "gzip")],
            b"00000000",
        ),
    )
    .await;
    assert_eq!(gz.status, 415);
    // A token in the query string is a 400.
    let q = raw(
        env.addr,
        request(
            "GET",
            &format!("/{WEB}.git/info/refs?service=git-receive-pack&token={WRITE}"),
            env.addr,
            None,
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(q.status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nearly_full_disk_refuses_the_push_with_503() {
    let env = Env::start("min_free_disk = \"900000GiB\"", "").await;
    let r = raw(
        env.addr,
        request(
            "POST",
            &format!("/{WEB}.git/git-receive-pack"),
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE],
            b"00000000",
        ),
    )
    .await;
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(r.text().contains("server disk is low"), "{}", r.text());
    assert!(r.header("retry-after").is_some());
}

// ------------------------------------------------------------------------------ tests: options

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_options_ttl_land_and_pass_through() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    git.commit(&a, "f", "one\n", "one").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);

    // `asc.ttl` on an agent branch sets an expiry; options are forwarded to the pipeline.
    let before = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let r = git
        .run(
            &a,
            &[
                "push",
                "-o",
                "asc.ttl=1d",
                "-o",
                "topic=t",
                "origin",
                "HEAD:refs/heads/agent/bot",
            ],
        )
        .await;
    assert!(r.ok, "{}", r.stderr);
    let entry = env
        .ref_entry(&env.web, "refs/heads/agent/bot")
        .await
        .unwrap();
    let expires = entry.expires_at.expect("a ttl was set");
    assert!(
        (before + 86_000..=before + 87_000).contains(&expires),
        "{expires}"
    );
    let pre = env.recorder.pre.lock().unwrap().clone();
    let last = pre.last().unwrap();
    assert_eq!(last.2, ["asc.ttl=1d", "topic=t"]);

    // A ttl on a shared branch is refused with the pipeline's reason; nothing moves.
    git.commit(&a, "f", "two\n", "two").await;
    let r = git
        .run(&a, &["push", "-o", "asc.ttl=1d", "origin", "master"])
        .await;
    assert!(!r.ok);
    assert!(remote_says(&r.stderr, "asc:"), "{}", r.stderr);
    assert!(r.stderr.contains("ttl"), "{}", r.stderr);
    // A malformed ttl is refused before any import.
    let r = git
        .run(
            &a,
            &[
                "push",
                "-o",
                "asc.ttl=soon",
                "origin",
                "HEAD:refs/heads/agent/x",
            ],
        )
        .await;
    assert!(!r.ok);
    assert!(
        r.stderr.contains("asc.ttl is not a duration"),
        "{}",
        r.stderr
    );
    assert!(
        env.ref_target(&env.web, "refs/heads/agent/x")
            .await
            .is_none()
    );
    // `-o land` needs the asc client.
    let r = git
        .run(&a, &["push", "-o", "land", "origin", "master"])
        .await;
    assert!(!r.ok);
    assert!(
        r.stderr.contains("asc: -o land needs the asc client"),
        "{}",
        r.stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_forge_pre_receive_reason_reaches_the_client_and_nothing_changes() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    let c1 = git.commit(&a, "f", "one\n", "one").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    *env.recorder.refuse.lock().unwrap() = Some("protected branch master".into());
    git.commit(&a, "f", "two\n", "two").await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    assert!(
        remote_says(&r.stderr, "asc: protected branch master"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("pre-receive hook declined"),
        "{}",
        r.stderr
    );
    assert_eq!(env.git_of(&env.web, "refs/heads/master").await, Some(c1));
    assert_eq!(env.ref_log(&env.web).await.len(), 1);
}

// ------------------------------------------------------------------------------ tests: paths

/// Crafted histories that must never be imported: git's own fsck refuses them at receive time
/// (`hasDotgit`, `gitmodules*`), in both `fsck` modes, and nothing of them is indexed.
async fn refused_tree(env: &Env, git: &Git, label: &str, tree: impl AsyncFnOnce(&Path) -> String) {
    let dir = git.repo(label, &env.url(WEB, WRITE)).await;
    let tree = tree(&dir).await;
    let commit = git.commit_tree(&dir, &tree, None, "crafted").await;
    let r = git
        .run(
            &dir,
            &["push", "origin", &format!("{commit}:refs/heads/master")],
        )
        .await;
    assert!(!r.ok, "{label}: the push was accepted\n{}", r.stderr);
    env.assert_empty(&env.web).await;
}

async fn aliases_and_gitmodules_are_refused(tables: &str) {
    let env = Env::start("", tables).await;
    let git = Git::new();
    for (label, name) in [
        ("dotgit", ".git"),
        ("dotgit-upper", ".GIT"),
        ("short-name", "git~1"),
        ("stream", ".git::$INDEX_ALLOCATION"),
    ] {
        refused_tree(&env, &git, label, async |dir: &Path| {
            let blob = git.object(dir, "blob", b"[core]\n").await;
            let inner = git.tree(dir, &[("100644", "config", &blob)]).await;
            git.tree(dir, &[("40000", name, &inner)]).await
        })
        .await;
    }
    // `.gitmodules` as a link, and with values that run programs or options.
    refused_tree(&env, &git, "modules-link", async |dir: &Path| {
        let blob = git.object(dir, "blob", b"/etc/passwd").await;
        git.tree(dir, &[("120000", ".gitmodules", &blob)]).await
    })
    .await;
    for (label, body) in [
        (
            "modules-url",
            "[submodule \"x\"]\n\tpath = sub\n\turl = -u./payload\n",
        ),
        (
            "modules-update",
            "[submodule \"x\"]\n\tpath = sub\n\turl = https://h/r\n\tupdate = !sh -c evil\n",
        ),
        (
            "modules-path",
            "[submodule \"x\"]\n\tpath = -evil\n\turl = https://h/r\n",
        ),
    ] {
        refused_tree(&env, &git, label, async |dir: &Path| {
            let blob = git.object(dir, "blob", body.as_bytes()).await;
            git.tree(dir, &[("100644", ".gitmodules", &blob)]).await
        })
        .await;
    }
    // Git refused them before the hook: asc's own policy was never asked.
    assert!(env.recorder.pre.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dot_git_aliases_and_dangerous_gitmodules_are_refused_with_nothing_imported() {
    aliases_and_gitmodules_are_refused("").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_migration_fsck_mode_never_relaxes_the_dangerous_classes() {
    aliases_and_gitmodules_are_refused("[git]\nfsck = \"migration\"").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_names_and_case_collisions_follow_the_import_and_native_rules() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let url = env.url(LIB, WRITE);
    let dir = git.repo("x", &url).await;
    let blob = git.object(&dir, "blob", b"x\n").await;

    // The first push to an empty repo uses the import rule: a history from another host is
    // never rejected for a reserved name or a case collision.
    let t1 = git
        .tree(
            &dir,
            &[
                ("100644", "A.txt", &blob),
                ("100644", "CON", &blob),
                ("100644", "a.txt", &blob),
            ],
        )
        .await;
    let c1 = git.commit_tree(&dir, &t1, None, "import").await;
    let r = git
        .run(
            &dir,
            &["push", "origin", &format!("{c1}:refs/heads/master")],
        )
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(
        env.git_of(&env.lib, "refs/heads/master").await,
        Some(c1.clone())
    );

    // Later pushes use the native rule for every new changeset.
    let t2 = git
        .tree(
            &dir,
            &[
                ("100644", "A.txt", &blob),
                ("100644", "CON", &blob),
                ("100644", "PRN", &blob),
                ("100644", "a.txt", &blob),
            ],
        )
        .await;
    let reserved = git.commit_tree(&dir, &t2, Some(&c1), "reserved").await;
    let r = git
        .run(
            &dir,
            &["push", "origin", &format!("{reserved}:refs/heads/master")],
        )
        .await;
    assert!(!r.ok);
    assert!(r.stderr.contains("refusing path 'PRN'"), "{}", r.stderr);
    let t3 = git
        .tree(
            &dir,
            &[
                ("100644", "A.txt", &blob),
                ("100644", "B.txt", &blob),
                ("100644", "CON", &blob),
                ("100644", "a.txt", &blob),
                ("100644", "b.txt", &blob),
            ],
        )
        .await;
    let collide = git.commit_tree(&dir, &t3, Some(&c1), "collide").await;
    let r = git
        .run(
            &dir,
            &["push", "origin", &format!("{collide}:refs/heads/master")],
        )
        .await;
    assert!(!r.ok);
    assert!(r.stderr.contains("refusing path"), "{}", r.stderr);
    assert_eq!(env.git_of(&env.lib, "refs/heads/master").await, Some(c1));
}

// ------------------------------------------------------------------------------ tests: limits

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_many_objects_and_an_object_that_is_too_large_are_refused_before_import() {
    let env = Env::start("max_push_objects = 20\nmax_object_bytes = \"1MiB\"", "").await;
    let git = Git::new();
    let a = git.repo("a", &env.url(WEB, WRITE)).await;

    // 30 files: 30 blobs, a tree and a commit.
    for i in 0..30 {
        std::fs::write(a.join(format!("f{i}.txt")), format!("file {i}\n")).unwrap();
    }
    git.ok(&a, &["add", "--all"]).await;
    git.ok(&a, &["commit", "-q", "-m", "many"]).await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    assert!(
        remote_says(&r.stderr, "asc: push has 32 objects, the limit is 20"),
        "{}",
        r.stderr
    );
    env.assert_empty(&env.web).await;

    // One blob of 3 MiB of zeros: a few KiB on the wire, 3 MiB inflated.
    let b = git.repo("b", &env.url(WEB, WRITE)).await;
    std::fs::write(b.join("zeros.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
    git.ok(&b, &["add", "--all"]).await;
    git.ok(&b, &["commit", "-q", "-m", "zeros"]).await;
    let r = git.run(&b, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    assert!(
        remote_says(
            &r.stderr,
            "asc: push has an object of 3145728 bytes, the limit is 1048576"
        ),
        "{}",
        r.stderr
    );
    env.assert_empty(&env.web).await;
    // The refusals came from the hook's own check, not from the pipeline.
    assert!(
        env.recorder.pre.lock().unwrap().is_empty(),
        "import was never reached"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_byte_limit_is_git_s_and_the_body_cap_is_ours() {
    let env = Env::start("max_push_bytes = \"2MiB\"\nmax_object_bytes = \"1MiB\"", "").await;
    let git = Git::new();

    // 3 MiB of noise exceeds `receive.maxInputSize` (git refuses it) but fits the body cap.
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    std::fs::write(a.join("noise.bin"), noise(3 * 1024 * 1024, 7)).unwrap();
    git.ok(&a, &["add", "--all"]).await;
    git.ok(&a, &["commit", "-q", "-m", "noise"]).await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    env.assert_empty(&env.web).await;

    // 12 MiB of noise: git's own limit stops it at 2 MiB, whatever the body cap is.
    let b = git.repo("b", &env.url(WEB, WRITE)).await;
    std::fs::write(b.join("noise.bin"), noise(12 * 1024 * 1024, 9)).unwrap();
    git.ok(&b, &["add", "--all"]).await;
    git.ok(&b, &["commit", "-q", "-m", "noise"]).await;
    let r = git.run(&b, &["push", "origin", "master"]).await;
    assert!(!r.ok);
    env.assert_empty(&env.web).await;

    // A body that is over the body cap (2 MiB + 8 MiB) is a 413 even when git refused it at its
    // first byte and stopped reading: the rest of the body is counted, never forwarded.
    let cap = 2 * 1024 * 1024 + 8 * 1024 * 1024;
    let r = raw(
        env.addr,
        request(
            "POST",
            &format!("/{WEB}.git/git-receive-pack"),
            env.addr,
            Some(&basic(WRITE)),
            &[PUSH_TYPE],
            &vec![b'x'; cap + 1],
        ),
    )
    .await;
    assert_eq!(r.status, 413, "{}", r.head);
    env.assert_empty(&env.web).await;

    // The server is fine afterwards.
    let c = git.repo("c", &env.url(WEB, WRITE)).await;
    git.commit(&c, "small", "ok\n", "small").await;
    let r = git.run(&c, &["push", "origin", "master"]).await;
    assert!(r.ok, "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_that_inflates_past_the_child_memory_cap_fails_the_push_and_no_more() {
    let env = Env::start("child_memory = \"160MiB\"", "").await;
    let git = Git::new();
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    // 400 MiB of zeros deflates to under 1 MiB, and needs 400 MiB to unpack.
    let file = std::fs::File::create(a.join("bomb.bin")).unwrap();
    file.set_len(400 * 1024 * 1024).unwrap();
    drop(file);
    git.ok(&a, &["add", "--all"]).await;
    git.ok(&a, &["commit", "-q", "-m", "bomb"]).await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(!r.ok, "the push of a memory bomb was accepted");
    env.assert_empty(&env.web).await;
    // The server still takes a normal push.
    let b = git.repo("b", &env.url(WEB, WRITE)).await;
    git.commit(&b, "small", "ok\n", "small").await;
    let r = git.run(&b, &["push", "origin", "master"]).await;
    assert!(r.ok, "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_that_dies_mid_pack_leaves_nothing_after_the_janitor_run() {
    let env = Env::start("", "").await;
    let git = Git::new();
    let a = git.repo("a", &env.url(WEB, WRITE)).await;
    std::fs::write(a.join("noise.bin"), noise(600_000, 3)).unwrap();
    git.ok(&a, &["add", "--all"]).await;
    git.ok(&a, &["commit", "-q", "-m", "noisy"]).await;
    let head = git.ok(&a, &["rev-parse", "HEAD"]).await;

    // The request of a push of `head`, cut in the middle of the pack.
    let mut body = pkt(format!(
        "{} {head} refs/heads/master\0 report-status side-band-64k agent=t\n",
        "0".repeat(40)
    )
    .as_bytes());
    body.extend_from_slice(b"0000");
    let pack = tokio::task::block_in_place(|| {
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
    });
    let full = body.len() + pack.len();
    body.extend_from_slice(&pack[..pack.len() / 2]);
    let head_text = String::from_utf8(request(
        "POST",
        &format!("/{WEB}.git/git-receive-pack"),
        env.addr,
        Some(&basic(WRITE)),
        &[PUSH_TYPE],
        b"",
    ))
    .unwrap()
    .replace("Content-Length: 0", &format!("Content-Length: {full}"));
    let mut s = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    s.write_all(head_text.as_bytes()).await.unwrap();
    s.write_all(&body).await.unwrap();
    s.flush().await.unwrap();
    drop(s);

    // Git ends on the short pack; nothing was imported or moved.
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
        if started.elapsed() > Duration::from_secs(8) && quarantines().is_empty() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "left: {:?}",
            quarantines()
        );
    }
    env.assert_empty(&env.web).await;
    assert!(!env.mapped(&env.web, &head).await);

    // The janitor removes a quarantine a killed child left, and only one that is old enough.
    let stale = objects.join("tmp_objdir-incoming-STALE");
    std::fs::create_dir_all(stale.join("pack")).unwrap();
    std::fs::create_dir_all(objects.join("keepme")).unwrap();
    assert_eq!(
        sweep_quarantines(env.views.view_dir(), Duration::from_secs(24 * 3600)),
        0
    );
    assert!(stale.exists(), "a fresh quarantine is not touched");
    assert_eq!(sweep_quarantines(env.views.view_dir(), Duration::ZERO), 1);
    assert!(!stale.exists());
    assert!(
        objects.join("keepme").exists(),
        "only quarantines are removed"
    );

    // The slot of the dead push was released.
    let b = git.repo("b", &env.url(WEB, WRITE)).await;
    git.commit(&b, "small", "ok\n", "small").await;
    assert!(git.run(&b, &["push", "origin", "master"]).await.ok);
}

// ------------------------------------------------------------------------------ tests: the hook socket

fn hook_request(token: &str, quarantine: &Path, new: &str) -> HookRequest {
    HookRequest {
        token: token.to_owned(),
        env: vec![(
            "GIT_QUARANTINE_PATH".into(),
            quarantine.to_string_lossy().into_owned(),
        )],
        commands: vec![HookCommand {
            old: None,
            new: Some(new.parse().unwrap()),
            name: "refs/heads/x".into(),
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hook_socket_rejects_replayed_expired_unknown_and_foreign_tokens() {
    // Tokens live for the request deadline at most: 4 seconds here.
    let env = Env::start("request_timeout = 4\nidle_timeout = 4", "").await;
    let push = GitPush::start(
        env.views.clone(),
        env.registry.clone(),
        env.ext.pipeline.clone(),
        &hook_program(),
    )
    .unwrap();
    let web_view = env.views.view(&env.web).unwrap();
    let lib_view = env.views.view(&env.lib).unwrap();
    let ctx = |repo: &RepoEntry| CallCtx {
        repo: repo.name.clone(),
        id_mode: IdMode::Asc,
        principal: Principal {
            id: "writer".into(),
        },
        op_id: Some(Uuid::new_v4()),
        client: None,
    };
    let quarantine = |view: &asc_git::compat::GitView| {
        let q = view.path().join("objects").join("tmp_objdir-incoming-t");
        std::fs::create_dir_all(&q).unwrap();
        q
    };
    let bogus = "1".repeat(40);
    let present = |token: &str, q: &Path| {
        let input = HookInput {
            endpoint: push.endpoint().to_owned(),
            request: hook_request(token, q, &bogus),
        };
        async move { asc_serve::git_push::request_hook(&input).await.unwrap() }
    };
    let token_of = |s: &asc_serve::git_push::PushSession| {
        s.env()
            .iter()
            .find(|(k, _)| *k == ENV_TOKEN)
            .unwrap()
            .1
            .to_owned()
    };

    // A token works once: the first presentation is processed (and fails on the missing object),
    // the replay is refused as a token.
    let s = push
        .begin(ctx(&env.web), env.web.clone(), web_view.clone())
        .unwrap();
    let token = token_of(&s);
    let q = quarantine(&web_view);
    let first = present(&token, &q).await;
    assert!(!first.accepted);
    assert_ne!(first.message, TOKEN_REFUSED, "{}", first.message);
    let replay = present(&token, &q).await;
    assert_eq!(
        (replay.accepted, replay.message.as_str()),
        (false, TOKEN_REFUSED)
    );
    assert_eq!(push.live_sessions(), 0);

    // A token for another repo's quarantine: refused, and the token is spent.
    let s = push
        .begin(ctx(&env.web), env.web.clone(), web_view.clone())
        .unwrap();
    let foreign = present(&token_of(&s), &quarantine(&lib_view)).await;
    assert!(!foreign.accepted);
    assert!(
        foreign.message.contains("does not name git's quarantine"),
        "{}",
        foreign.message
    );
    assert_eq!(push.live_sessions(), 0);

    // An unknown token.
    let unknown = present(&"ab".repeat(32), &q).await;
    assert_eq!(unknown.message, TOKEN_REFUSED);
    // Dropping the session (the request ended) voids the token.
    let s = push
        .begin(ctx(&env.web), env.web.clone(), web_view.clone())
        .unwrap();
    let token = token_of(&s);
    drop(s);
    assert_eq!(present(&token, &q).await.message, TOKEN_REFUSED);

    // An expired token.
    let s = push
        .begin(ctx(&env.web), env.web.clone(), web_view.clone())
        .unwrap();
    let token = token_of(&s);
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert_eq!(present(&token, &q).await.message, TOKEN_REFUSED);
    drop(s);

    // Garbage on the socket is declined, not served.
    let endpoint = asc_rpc::ipc::IpcEndpoint::from_raw(push.endpoint().to_owned());
    let mut stream = asc_rpc::ipc::connect_stream(&endpoint).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\n\r\n\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut got)).await;
    if got.len() >= 9 {
        assert_eq!(got[8], 1, "a reply to garbage must decline");
    }
}
