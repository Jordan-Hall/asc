//! `bridge_promote`: the outbox effect that keeps the host's bare git directory (the bridge)
//! current after a write through the asc pipeline, so that a plain `git clone` of the host sees
//! it. Real git is the client of asc's git HTTP (the write path) and reads the bridge back.
//!
//! The server is the production wiring of `git_push.rs` (`GitViewService`, `git_http`, the
//! credential edge), plus an `Outbox` that holds the real `BridgePromoteEffect`; the stand-in
//! pipeline hooks list `bridge_promote` the way `ForgejoHooks` does under
//! `[forgejo] bridge_promote`.
#![cfg(feature = "git-compat")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use asc_git::realgit::RealGit;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::{RepoName, ServerInfo};
use asc_serve::config::ServeConfig;
use asc_serve::git_push::HookProgram;
use asc_serve::git_view::{BridgePromoteEffect, GitViewService};
use asc_serve::http::Metrics;
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::outbox::{Effect, Job, Outbox, OutboxConfig};
use asc_serve::registry::{Extensions, NewRepo, Registry};
use asc_serve::update::PushHooks;
use asc_serve::{git_http, migrate};
use async_trait::async_trait;
use sqlx::Row;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use uuid::Uuid;

const WRITE: &str = "tok-write-S3CR3T";
const WEB: &str = "Acme/Web";
/// Set in the environment of the hook process only (by the `pre-receive` script's `receive-pack`).
const HOOK_TOKEN_VAR: &str = "ASC_GIT_HOOK_TOKEN";

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
}

// ------------------------------------------------------------------------------ the server

struct Acl;

#[async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        _access: Access,
    ) -> Result<Principal, AuthError> {
        match token {
            Some(t) if t.expose() == WRITE && repo == WEB => Ok(Principal {
                id: "writer".to_owned(),
            }),
            Some(_) => Err(AuthError::Forbidden),
            None => Err(AuthError::Unauthenticated),
        }
    }
}

/// The pipeline hooks of a host with `[forgejo] bridge_promote`: promote first, then tell the
/// forge.
struct PromotingHooks;

#[async_trait]
impl PushHooks for PromotingHooks {
    fn outbox_effects(&self) -> &'static [&'static str] {
        &["bridge_promote", "post_receive"]
    }
}

struct Env {
    registry: Arc<Registry>,
    repo_id: i64,
    addr: SocketAddr,
    outbox: Arc<Outbox>,
    effect: Arc<BridgePromoteEffect>,
    /// `<root>/acme/web.git`, where the host keeps the bridge (lowercased).
    bridge: PathBuf,
    root: PathBuf,
    git: Git,
    _keep: (tempfile::TempDir, Extensions),
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

impl Env {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let db = format!("sqlite:{}?mode=rwc", fwd(&dir.path().join("asc.db")));
        let cfg = ServeConfig::parse(&format!(
            "store = \"memory:///\"\ndb = {db:?}\ncache = {{ disk_dir = {:?}, disk = \"1GiB\" }}\n\
             [limits.git]\nmin_free_disk = \"1KiB\"\n",
            fwd(&dir.path().join("cache"))
        ))
        .unwrap();
        let pool = migrate::connect(&db, 4).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
        let repo_id = registry
            .create(&RepoName::parse(WEB).unwrap(), &NewRepo::default())
            .await
            .unwrap()
            .key
            .0;
        let metrics = Arc::new(Metrics::new());
        let ext = Extensions::new();
        assert!(
            ext.pipeline
                .set_push_hooks(Arc::new(PromotingHooks))
                .is_ok()
        );
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

        // The host's repository root, and the bare directory it made for the repository.
        let root = dir.path().join("repositories");
        let bridge = root.join("acme").join("web.git");
        let git = Git::new();
        std::fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        git.ok(
            dir.path(),
            &["init", "--bare", "-q", &bridge.to_string_lossy()],
        )
        .await;

        let outbox = Arc::new(Outbox::new(pool, metrics, OutboxConfig::default()));
        let effect = Arc::new(BridgePromoteEffect::new(
            views,
            registry.clone(),
            root.clone(),
        ));
        outbox.effects().register(effect.clone());
        Self {
            registry,
            repo_id,
            addr,
            outbox,
            effect,
            bridge,
            root,
            git,
            _keep: (dir, ext),
        }
    }

    fn url(&self) -> String {
        format!("http://x-access-token:{WRITE}@{}/{WEB}.git", self.addr)
    }

    /// Runs the outbox until nothing is due.
    async fn deliver(&self) {
        while self.outbox.deliver_due(now()).await.unwrap() > 0 {}
    }

    async fn undelivered(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) FROM outbox WHERE done_at IS NULL")
            .fetch_one(self.registry.pool())
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }

    /// The commit `name` points at in the bridge, as a reader of the bridge sees it.
    async fn bridge_ref(&self, name: &str) -> Option<String> {
        let r = self
            .git
            .run(
                &self.bridge,
                &["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")],
            )
            .await;
        r.ok.then(|| r.stdout.trim().to_owned())
    }

    async fn clone_bridge(&self, into: &str) -> PathBuf {
        let dir = self.git.work(into);
        let r = self
            .git
            .run(&dir, &["clone", "-q", &self.bridge.to_string_lossy(), "c"])
            .await;
        assert!(r.ok, "clone of the bridge: {}", r.stderr);
        dir.join("c")
    }

    fn job(&self, refs: &[&str]) -> Job {
        Job {
            id: 1,
            repo_id: self.repo_id,
            op_id: Uuid::new_v4().as_bytes().to_vec(),
            doc: serde_json::json!({
                "effects": ["bridge_promote"],
                "refs": refs.iter().map(|n| serde_json::json!({ "name": n })).collect::<Vec<_>>(),
            }),
        }
    }
}

// ------------------------------------------------------------------------------ the tests

#[tokio::test(flavor = "multi_thread")]
async fn a_write_through_the_pipeline_reaches_a_plain_clone_of_the_bridge() {
    let env = Env::start().await;
    let git = &env.git;
    let a = git.repo("a", &env.url()).await;

    // The first push through asc's git HTTP. Its outbox row lists bridge_promote before
    // post_receive.
    let c1 = git.commit(&a, "README", "one\n", "one").await;
    let r = git.run(&a, &["push", "origin", "master"]).await;
    assert!(r.ok, "push: {}", r.stderr);
    let effects: Vec<u8> = sqlx::query("SELECT effects FROM outbox ORDER BY id LIMIT 1")
        .fetch_one(env.registry.pool())
        .await
        .unwrap()
        .try_get("effects")
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&effects).unwrap();
    assert_eq!(
        doc["effects"],
        serde_json::json!(["bridge_promote", "post_receive"])
    );

    // Before the outbox runs the bridge has nothing; after it, a plain clone has the commit.
    assert_eq!(env.bridge_ref("refs/heads/master").await, None);
    env.deliver().await;
    assert_eq!(env.undelivered().await, 0);
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1.clone()));
    let clone = env.clone_bridge("b1").await;
    assert_eq!(git.ok(&clone, &["rev-parse", "HEAD"]).await, c1);
    assert_eq!(
        std::fs::read_to_string(clone.join("README")).unwrap(),
        "one\n"
    );
    assert!(git.run(&env.bridge, &["fsck", "--strict"]).await.ok);

    // A fast-forward and a new branch, then a deletion: the bridge follows asc's refs.
    let c2 = git.commit(&a, "src/lib.txt", "two\n", "two").await;
    git.ok(&a, &["branch", "topic"]).await;
    let r = git.run(&a, &["push", "origin", "master", "topic"]).await;
    assert!(r.ok, "push 2: {}", r.stderr);
    env.deliver().await;
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c2.clone()));
    assert_eq!(env.bridge_ref("refs/heads/topic").await, Some(c2.clone()));
    let r = git.run(&a, &["push", "origin", "--delete", "topic"]).await;
    assert!(r.ok, "delete: {}", r.stderr);
    env.deliver().await;
    assert_eq!(env.bridge_ref("refs/heads/topic").await, None);
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c2.clone()));

    // The same commit comes through asc's own git HTTP, and a fetch of the bridge agrees.
    let via_http = git.work("http");
    let r = git.run(&via_http, &["clone", "-q", &env.url(), "c"]).await;
    assert!(r.ok, "clone through asc: {}", r.stderr);
    assert_eq!(
        git.ok(&via_http.join("c"), &["rev-parse", "HEAD"]).await,
        c2
    );
    git.ok(&clone, &["fetch", "-q", "origin"]).await;
    assert_eq!(git.ok(&clone, &["rev-parse", "origin/master"]).await, c2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_delivery_changes_nothing_and_a_row_of_the_hosts_own_git_waits_for_it_then_makes_the_directory_right()
 {
    let env = Env::start().await;
    let git = &env.git;
    let a = git.repo("a", &env.url()).await;
    let c1 = git.commit(&a, "f", "one\n", "one").await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    env.deliver().await;
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1.clone()));

    // At least once: the same row again finds everything in place.
    let again = env.job(&["refs/heads/master"]);
    env.effect.run(&again).await.unwrap();
    env.effect.run(&again).await.unwrap();
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1.clone()));

    // A push the host's own git finishes (ImportGitPush) moves the bridge ref itself: the effect
    // must not race it. It waits for that git instead (the row stays, to be tried again).
    let c2 = git
        .commit(
            &a, "f", "two
", "two",
        )
        .await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    sqlx::query(
        "UPDATE ref_log SET via = 'host-git-push' WHERE seq = (SELECT MAX(seq) FROM ref_log)",
    )
    .execute(env.registry.pool())
    .await
    .unwrap();
    env.deliver().await;
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1.clone()));
    assert_eq!(
        env.undelivered().await,
        1,
        "the row waits for the host's git"
    );

    // The host's git never finished that push (it was killed after its hook, or the host restarted), and
    // asc holds the commit: after the tries it gives, the row makes the directory right itself.
    sqlx::query("UPDATE outbox SET attempts = 3, next_at = 0 WHERE done_at IS NULL")
        .execute(env.registry.pool())
        .await
        .unwrap();
    env.deliver().await;
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c2.clone()));
    assert_eq!(env.undelivered().await, 0);

    // A push the host's git did finish: the directory already holds the ref and its commit, and the
    // row passes at once, without a try more.
    let c3 = git
        .commit(
            &a, "f", "three
", "three",
        )
        .await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    sqlx::query(
        "UPDATE ref_log SET via = 'host-git-push' WHERE seq = (SELECT MAX(seq) FROM ref_log)",
    )
    .execute(env.registry.pool())
    .await
    .unwrap();
    let finished = env.job(&["refs/heads/master"]);
    env.effect.run(&finished).await.unwrap(); // stands for the host's git moving the ref
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c3.clone()));
    env.deliver().await;
    assert_eq!(env.undelivered().await, 0);
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c3));
}

/// The state of the bug seen on staging: a ref of the host's directory names a commit the directory does not
/// hold (or a ref asc has is missing). Nothing was queued for it, so only a look at the directory finds it.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_that_lost_commits_or_refs_is_made_right_at_start() {
    let env = Env::start().await;
    let git = &env.git;
    let a = git.repo("a", &env.url()).await;
    let c1 = git
        .commit(
            &a, "f", "one
", "one",
        )
        .await;
    assert!(git.run(&a, &["push", "origin", "master"]).await.ok);
    env.deliver().await;
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1.clone()));

    // The directory is right: nothing to do.
    assert_eq!(env.effect.clone().reconcile_all().await, 0);

    // The commits are gone while the ref stays (the push was never finished by the host's git).
    for entry in std::fs::read_dir(env.bridge.join("objects")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() && path.file_name().is_some_and(|n| n.len() == 2) {
            std::fs::remove_dir_all(path).unwrap();
        }
    }
    assert!(!git.run(&env.bridge, &["cat-file", "-e", &c1]).await.ok);
    assert_eq!(env.effect.clone().reconcile_all().await, 1);
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1.clone()));
    let clone = env.clone_bridge("healed").await;
    assert_eq!(
        git.run(&clone, &["rev-parse", "HEAD"]).await.stdout.trim(),
        c1
    );

    // The ref is gone too (a restored volume).
    assert!(
        git.run(&env.bridge, &["update-ref", "-d", "refs/heads/master"])
            .await
            .ok
    );
    assert_eq!(env.bridge_ref("refs/heads/master").await, None);
    assert_eq!(env.effect.clone().reconcile_all().await, 1);
    assert_eq!(env.bridge_ref("refs/heads/master").await, Some(c1));
    assert_eq!(env.effect.clone().reconcile_all().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn private_refs_never_reach_the_bridge_and_failures_retry_without_leaking() {
    let env = Env::start().await;

    // Only private refs: nothing to do, not even a bridge directory is needed.
    let private = env.job(&["refs/users/7/heads/x", "refs/snapshots/s"]);
    env.effect.run(&private).await.unwrap();

    // A shared ref with no bridge directory is an error (the row is retried), and the message
    // names the repository and no credential.
    let shared = env.job(&["refs/heads/master"]);
    std::fs::remove_dir_all(&env.bridge).unwrap();
    let err = env.effect.run(&shared).await.unwrap_err().0;
    assert!(err.contains("Acme/Web"), "{err}");
    for needle in ["S3CR3T", "x-access-token", "Authorization"] {
        assert!(!err.contains(needle), "{err}");
    }

    // A directory that is not a bare repository is refused too.
    std::fs::create_dir_all(&env.bridge).unwrap();
    let err = env.effect.run(&shared).await.unwrap_err().0;
    assert!(err.contains("not a bare git repository"), "{err}");

    // A repository that asc no longer has needs no bridge.
    let gone = Job {
        repo_id: env.repo_id + 1000,
        ..env.job(&["refs/heads/master"])
    };
    env.effect.run(&gone).await.unwrap();
    assert!(env.root.is_absolute());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_directory_that_is_a_symlink_is_refused() {
    let env = Env::start().await;
    let elsewhere = env.root.join("elsewhere.git");
    std::fs::rename(&env.bridge, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &env.bridge).unwrap();
    let err = env
        .effect
        .run(&env.job(&["refs/heads/master"]))
        .await
        .unwrap_err()
        .0;
    assert!(err.contains("not a plain directory"), "{err}");
}
