//! The mirror migration over SSH (SPEC §15.6.3), and the whole-stack cases that need an
//! in-process server: Forgejo-shaped principals from a fake forge (user keys and deploy keys,
//! U85), the branchless cases of SPEC §25.6, mirror-mode repositories, one audit record per
//! login and command, and admission. The in-process server is the production wiring
//! (`GitViewService`, `git_http::register_with_hook`, `GitSshExec`, `SshServer`) with the
//! forge authorizer; real git and real OpenSSH are the clients.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use asc_rpc::server::RepoName;
use asc_serve::config::{ServeConfig, SshConfig, SshLimitsConfig};
use asc_serve::forgejo::{AuthzCacheConfig, ForgejoClient, InternalAuthz, InternalToken};
use asc_serve::git_push::HookProgram;
use asc_serve::git_view::GitViewService;
use asc_serve::http::Metrics;
use asc_serve::limits::Admission;
use asc_serve::registry::{Extensions, NewRepo, Registry, RepoMode};
use asc_serve::ssh::audit::{CommandRecord, LoginRecord, RefusedRecord};
use asc_serve::ssh::host_key::HostKeys;
use asc_serve::ssh::{
    Audit, AuditSink, GitAuditSink, GitRecord, GitSshExec, SshDeps, SshMetrics, SshServer,
};
use asc_serve::{git_http, migrate};
use serde_json::{Value, json};
use sqlx::Row;

use crate::h::*;
use crate::need_tools;

// =================================================================================== migration

/// A fixture with a merge, an annotated and a lightweight tag, an executable file, a symlink
/// entry, a second branch and an empty-tree commit.
fn migration_fixture(git: &Git) -> PathBuf {
    let w = git.repo_with_commits("old", 2);
    git.ok(&w, &["checkout", "-q", "-b", "side"]);
    git.commit(&w, "side.txt", "side\n", "side");
    git.ok(&w, &["checkout", "-q", "master"]);
    git.commit(&w, "main.txt", "main\n", "main");
    git.ok(&w, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    std::fs::write(w.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    git.ok(&w, &["add", "run.sh"]);
    git.ok(&w, &["update-index", "--chmod=+x", "run.sh"]);
    let target = {
        std::fs::write(w.join("target.txt"), "run.sh").unwrap();
        git.ok(&w, &["hash-object", "-w", "target.txt"])
    };
    std::fs::remove_file(w.join("target.txt")).unwrap();
    git.ok(
        &w,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{target},link"),
        ],
    );
    git.ok(&w, &["commit", "-q", "-m", "mode and link"]);
    git.ok(&w, &["tag", "-a", "v1", "-m", "release one"]);
    git.ok(&w, &["tag", "light"]);
    git.ok(&w, &["commit", "-q", "--allow-empty", "-m", "empty"]);
    w
}

#[test]
fn a_mirror_push_over_ssh_migrates_every_ref_and_sha() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let git = &rig.git;
    let cmd = rig.cmd(&rig.alice);
    let old = migration_fixture(git);
    let url = rig.stack.url("acme/web");

    git.ssh_ok(&old, &cmd, &["push", "-q", "--mirror", &url]);

    // git clones it back: zero mismatches.
    let back = git.work("back");
    git.ssh_ok(&back, &cmd, &["clone", "-q", "--mirror", &url, "back.git"]);
    assert_eq!(git.refs(&back.join("back.git")), git.refs(&old));
    let tree = |repo: &Path, rev: &str| git.ok(repo, &["ls-tree", "-r", rev]);
    assert_eq!(tree(&back.join("back.git"), "master"), tree(&old, "master"));
    assert!(
        tree(&old, "master").contains("100755"),
        "the fixture has an executable"
    );
    assert!(
        tree(&old, "master").contains("120000"),
        "the fixture has a link"
    );

    // asc clones it too and sees the same tip.
    let sb = crate::support::Sandbox::new();
    let dir = sb.dir("c");
    let mut c = sb.asc(&dir);
    c.env("GIT_SSH_COMMAND", &cmd)
        .env_remove("SSH_AUTH_SOCK")
        .args(["clone", "-q", &url, "w"])
        .stdin(Stdio::null());
    let out = c.output().unwrap();
    assert!(out.status.success(), "{}", mask(&lossy(&out.stderr)));
    let mut rp = sb.asc(&dir.join("w"));
    rp.env("GIT_SSH_COMMAND", &cmd).args(["rev-parse", "HEAD"]);
    let out = rp.output().unwrap();
    assert_eq!(lossy(&out.stdout).trim(), git.head(&old));
}

// =================================================================================== the fake forge

#[derive(Clone)]
struct KeyRow {
    id: i64,
    kind: &'static str,
    user: (i64, &'static str),
    usable: bool,
    /// repo -> permission (`read`, `write`); a deploy key's grants name its repositories.
    grants: HashMap<String, &'static str>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<HashMap<String, KeyRow>>>);

impl Fake {
    fn add(&self, key: &Key, row: KeyRow) {
        self.0.lock().unwrap().insert(key.blob(), row);
    }

    fn set_usable(&self, key: &Key, usable: bool) {
        self.0.lock().unwrap().get_mut(&key.blob()).unwrap().usable = usable;
    }
}

fn reply(status: u16, body: Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

const INTERNAL: &str = "internal-secret";

async fn forge(
    State(fake): State<Fake>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if auth != Some(&format!("Bearer {INTERNAL}")) {
        return reply(403, json!({}));
    }
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let rows = fake.0.lock().unwrap().clone();
    match uri.path() {
        "/api/internal/asc/ssh-key" => {
            let blob = json["key"].as_str().unwrap_or("");
            match rows.get(blob).filter(|r| r.usable) {
                Some(r) => {
                    let (uid, name) = if r.kind == "user" { r.user } else { (0, "") };
                    reply(
                        200,
                        json!({"known": true, "key_id": r.id, "kind": r.kind,
                               "user_id": uid, "username": name}),
                    )
                }
                None => reply(
                    200,
                    json!({"known": false, "key_id": 0, "kind": "", "user_id": 0, "username": ""}),
                ),
            }
        }
        "/api/internal/asc/authz" => {
            let blob = json["ssh_key"]["key"].as_str().unwrap_or("");
            let repo = format!(
                "{}/{}",
                json["owner"].as_str().unwrap_or(""),
                json["repo"].as_str().unwrap_or("")
            );
            let write = json["mode"].as_str() == Some("write");
            let denied = || reply(200, json!({"allowed": false}));
            let Some(row) = rows.get(blob).filter(|r| r.usable) else {
                return denied();
            };
            let Some(perm) = row.grants.get(&repo) else {
                return denied();
            };
            if write && *perm == "read" {
                return denied();
            }
            let (uid, name) = if row.kind == "user" {
                row.user
            } else {
                (1, "acme")
            };
            reply(
                200,
                json!({"allowed": true, "user_id": uid, "username": name,
                       "perm": perm, "deploy_key_id": if row.kind == "user" { 0 } else { row.id }}),
            )
        }
        _ => reply(404, json!({})),
    }
}

// =================================================================================== audit

#[derive(Default)]
struct Records {
    logins: Mutex<Vec<LoginRecord>>,
    refused: Mutex<Vec<RefusedRecord>>,
    commands: Mutex<Vec<CommandRecord>>,
    git: Mutex<Vec<GitRecord>>,
}

impl AuditSink for Records {
    fn login(&self, r: &LoginRecord) {
        self.logins.lock().unwrap().push(r.clone());
    }

    fn refused(&self, r: &RefusedRecord) {
        self.refused.lock().unwrap().push(r.clone());
    }

    fn command(&self, r: &CommandRecord) {
        self.commands.lock().unwrap().push(r.clone());
    }
}

impl GitAuditSink for Records {
    fn record(&self, r: &GitRecord) {
        self.git.lock().unwrap().push(r.clone());
    }
}

impl Records {
    /// Every record, formatted as the audit sinks would show it.
    fn dump(&self) -> String {
        format!(
            "{:?}{:?}{:?}{:?}",
            self.logins.lock().unwrap(),
            self.refused.lock().unwrap(),
            self.commands.lock().unwrap(),
            self.git.lock().unwrap()
        )
    }
}

// =================================================================================== the rig

struct InProc {
    ssh: SocketAddr,
    known_hosts: PathBuf,
    registry: Arc<Registry>,
    handle: tokio::runtime::Handle,
    fake: Fake,
    audit: Arc<Records>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    dir: tempfile::TempDir,
}

const WEB: &str = "acme/web";
const SOLO: &str = "acme/solo";
const MIRROR: &str = "acme/mirror";

impl InProc {
    fn start(limits: SshLimitsConfig, git_limits: &str) -> Self {
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let path = dir.path().to_owned();
        let git_limits = git_limits.to_owned();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .unwrap();
            let handle = rt.handle().clone();
            rt.block_on(async move {
                let db = format!("sqlite:{}?mode=rwc", fwd(&path.join("asc.db")));
                let cfg = ServeConfig::parse(&format!(
                    "store = \"memory:///\"\ndb = {db:?}\n\
                     cache = {{ disk_dir = {:?}, disk = \"1GiB\" }}\n\
                     [limits.git]\nmin_free_disk = \"1KiB\"\n{git_limits}\n",
                    fwd(&path.join("cache"))
                ))
                .unwrap();
                let pool = migrate::connect(&db, 4).await.unwrap();
                migrate::migrate(&pool).await.unwrap();
                let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
                for (name, mode) in [
                    (WEB, RepoMode::Primary),
                    (SOLO, RepoMode::Primary),
                    (MIRROR, RepoMode::Mirror),
                ] {
                    registry
                        .create(
                            &RepoName::parse(name).unwrap(),
                            &NewRepo {
                                mode,
                                default_branch: "master".into(),
                            },
                        )
                        .await
                        .unwrap();
                }
                sqlx::query("UPDATE repos SET branchless = TRUE WHERE name = $1")
                    .bind(SOLO)
                    .execute(registry.pool())
                    .await
                    .unwrap();

                // The fake forge.
                let fake = Fake::default();
                let app = Router::new().fallback(forge).with_state(fake.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let forge_url = format!("http://{}/", listener.local_addr().unwrap());
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                let client = Arc::new(
                    ForgejoClient::new(&forge_url, Some(InternalToken::new(INTERNAL).unwrap()))
                        .unwrap(),
                );
                let authz = Arc::new(InternalAuthz::new(
                    client,
                    Vec::new(),
                    AuthzCacheConfig {
                        positive: Duration::from_millis(50),
                        negative: Duration::from_millis(50),
                        entries: 256,
                    },
                ));

                let metrics = Arc::new(Metrics::new());
                let ext = Extensions::new();
                let views = GitViewService::start(&cfg, pool, None)
                    .unwrap()
                    .expect("the git layer is enabled");
                let hook = HookProgram {
                    exe: env!("CARGO_BIN_EXE_asc").into(),
                    args: ["serve", "git-hook", "pre-receive"]
                        .map(str::to_owned)
                        .to_vec(),
                };
                let services = git_http::register_with_hook(
                    &ext,
                    views,
                    registry.clone(),
                    &metrics,
                    &cfg,
                    &hook,
                );
                let admission = Arc::new(Admission::new(&cfg.limits));
                let audit = Arc::new(Records::default());
                let exec = GitSshExec::new(services, admission.clone()).with_audit(audit.clone());
                let host_key = path.join("ssh_host_ed25519_key");
                let ssh_cfg = SshConfig {
                    enabled: true,
                    listen: "127.0.0.1:0".parse().unwrap(),
                    host_key_files: vec![host_key.clone()],
                    ..SshConfig::default()
                };
                let server = SshServer::bind(
                    &ssh_cfg,
                    &limits,
                    SshDeps {
                        authorizer: authz,
                        admission: Some(admission),
                        audit: Audit::new(audit.clone(), SshMetrics::detached()),
                        exec: Arc::new(exec),
                    },
                )
                .await
                .expect("bind the ssh listener");
                let addr = server.local_addr().unwrap();
                let keys = HostKeys::load_existing(&[host_key]).unwrap();
                let shown = keys.show_text("127.0.0.1", addr.port());
                let line = shown
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("known_hosts:"))
                    .unwrap()
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ");
                let known = path.join("known_hosts");
                std::fs::write(&known, format!("[127.0.0.1]:{} {line}\n", addr.port())).unwrap();
                tokio::spawn(server.run(async {
                    let _ = stop_rx.await;
                }));
                tx.send((addr, known, registry, handle, fake, audit))
                    .unwrap();
                std::future::pending::<()>().await;
            });
        });
        let (ssh, known_hosts, registry, handle, fake, audit) = rx.recv().unwrap();
        Self {
            ssh,
            known_hosts,
            registry,
            handle,
            fake,
            audit,
            stop: Some(stop_tx),
            dir,
        }
    }

    fn url(&self, repo: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/{repo}.git", self.ssh.port())
    }

    fn cmd(&self, tools: &Tools, key: &Key) -> String {
        tools.command(None, Some(key), &self.known_hosts, &[])
    }

    fn count(&self, sql: &'static str) -> i64 {
        self.handle.block_on(async {
            sqlx::query(sql)
                .fetch_one(self.registry.pool())
                .await
                .unwrap()
                .try_get::<i64, _>(0)
                .unwrap()
        })
    }

    fn user_refs(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM refs WHERE name LIKE 'refs/users/%'")
    }

    fn changes(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM changes")
    }
}

impl Drop for InProc {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

fn user_row(id: i64, user: (i64, &'static str), grants: &[(&str, &'static str)]) -> KeyRow {
    KeyRow {
        id,
        kind: "user",
        user,
        usable: true,
        grants: grants.iter().map(|(r, p)| ((*r).to_owned(), *p)).collect(),
    }
}

fn deploy_row(id: i64, grants: &[(&str, &'static str)]) -> KeyRow {
    KeyRow {
        kind: "deploy",
        ..user_row(id, (0, ""), grants)
    }
}

fn quiet() -> SshLimitsConfig {
    SshLimitsConfig {
        reject_delay_ms: 0,
        ..SshLimitsConfig::default()
    }
}

// =================================================================================== forge keys

#[test]
fn forge_user_keys_and_deploy_keys_log_in_with_forge_rights() {
    let tools = need_tools!();
    let srv = InProc::start(quiet(), "");
    let git = Git::new();
    let keys = tempfile::tempdir().unwrap();
    let alice = tools.key(keys.path(), "alice", "ed25519");
    let bob = tools.key(keys.path(), "bob", "ecdsa");
    let deploy = tools.key(keys.path(), "deploy", "ed25519");
    let ghost = tools.key(keys.path(), "ghost", "ed25519");
    srv.fake.add(
        &alice,
        user_row(
            1,
            (42, "alice"),
            &[(WEB, "write"), (SOLO, "write"), (MIRROR, "write")],
        ),
    );
    srv.fake
        .add(&bob, user_row(2, (43, "bob"), &[(WEB, "read")]));
    srv.fake.add(&deploy, deploy_row(3, &[(WEB, "read")]));

    let src = git.repo_with_commits("src", 2);
    let acmd = srv.cmd(&tools, &alice);
    let r = git.ssh(&src, &acmd, &["push", "-q", &srv.url(WEB), "master"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    let sha = git.head(&src);

    // Reads: a read user key and a read deploy key clone; an unknown key does not.
    for (name, key) in [("bob", &bob), ("deploy", &deploy)] {
        let c = git.work(name);
        git.ssh_ok(
            &c,
            &srv.cmd(&tools, key),
            &["clone", "-q", &srv.url(WEB), "w"],
        );
        assert_eq!(git.head(&c.join("w")), sha, "{name}");
    }
    let r = git.ssh(
        &git.work("g"),
        &srv.cmd(&tools, &ghost),
        &["ls-remote", &srv.url(WEB)],
    );
    assert_eq!(r.code, Some(128));
    assert!(
        r.stderr.contains("Permission denied (publickey)"),
        "{}",
        r.stderr
    );

    // Writes need the forge's write permission.
    let w = git.work("bob").join("w");
    git.commit(&w, "b.txt", "b\n", "b");
    let r = git.ssh(&w, &srv.cmd(&tools, &bob), &["push", "origin", "master"]);
    assert!(
        !r.ok && r.stderr.contains("read-only access"),
        "{}",
        mask(&r.stderr)
    );

    // A deploy key reaches only its repository.
    let r = git.ssh(
        &git.work("d"),
        &srv.cmd(&tools, &deploy),
        &["ls-remote", &srv.url(SOLO)],
    );
    assert_eq!(r.code, Some(128));
    assert!(
        r.stderr.contains("repository not found"),
        "{}",
        mask(&r.stderr)
    );

    // The forge disables the key: the next login is refused.
    srv.fake.set_usable(&alice, false);
    std::thread::sleep(Duration::from_millis(300));
    let r = git.ssh(&src, &acmd, &["ls-remote", &srv.url(WEB)]);
    assert_eq!(r.code, Some(128));
    assert!(
        r.stderr.contains("Permission denied (publickey)"),
        "{}",
        r.stderr
    );
}

// =================================================================================== branchless

#[test]
fn branchless_pushes_over_ssh_follow_the_spec() {
    let tools = need_tools!();
    let srv = InProc::start(quiet(), "");
    let git = Git::new();
    let keys = tempfile::tempdir().unwrap();
    let alice = tools.key(keys.path(), "alice", "ed25519");
    let carol = tools.key(keys.path(), "carol", "ed25519");
    srv.fake
        .add(&alice, user_row(1, (7, "alice"), &[(SOLO, "write")]));
    srv.fake
        .add(&carol, user_row(2, (8, "carol"), &[(SOLO, "write")]));
    let (acmd, ccmd) = (srv.cmd(&tools, &alice), srv.cmd(&tools, &carol));
    let url = srv.url(SOLO);

    let a = git.repo_with_commits("a", 1);
    git.ok(&a, &["remote", "add", "origin", &url]);
    let r = git.ssh(&a, &acmd, &["push", "origin", "master"]);
    assert!(r.ok, "trunk push: {}", mask(&r.stderr));

    // A plain push of a non-shared branch becomes the pusher's user ref; no change.
    git.ok(&a, &["checkout", "-q", "-b", "feat"]);
    git.commit(&a, "feat.txt", "f\n", "feat");
    let (users, changes) = (srv.user_refs(), srv.changes());
    let r = git.ssh(&a, &acmd, &["push", "origin", "HEAD"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert!(
        r.stderr
            .contains("remote: asc: stored as your branch 'feat'"),
        "{}",
        mask(&r.stderr)
    );
    assert_eq!(srv.user_refs(), users + 1);
    assert_eq!(srv.changes(), changes, "no change from a plain push");

    // The user ref is advertised to nobody, the pusher included.
    for cmd in [&acmd, &ccmd] {
        let ls = git.ssh_ok(&git.work("ls"), cmd, &["ls-remote", &url]);
        assert!(!ls.contains("refs/users") && !ls.contains("feat"), "{ls}");
        assert!(ls.contains("refs/heads/master"), "{ls}");
    }
    // A second principal pushing the same name gets a ref of their own.
    let c = git.work("c");
    git.ssh_ok(&c, &ccmd, &["clone", "-q", &url, "w"]);
    let cw = c.join("w");
    git.ok(&cw, &["checkout", "-q", "-b", "feat"]);
    git.commit(&cw, "carol.txt", "c\n", "carol feat");
    let r = git.ssh(&cw, &ccmd, &["push", "origin", "HEAD"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert_eq!(srv.user_refs(), users + 2);

    // refs/for/<trunk> needs the forge's proc-receive (C34): refused here, as over HTTP.
    let r = git.ssh(&a, &acmd, &["push", "origin", "HEAD:refs/for/master"]);
    assert!(!r.ok, "refs/for without the forge is refused");
    assert!(
        r.stderr
            .contains("remote: asc: refs/for/master is not a shared ref"),
        "{}",
        mask(&r.stderr)
    );
    assert_eq!(srv.changes(), changes);

    // git cannot name a ref it is not shown, so the deletion stays with the SPEC's v1.x text.
    let before = srv.user_refs();
    let r = git.ssh(&a, &acmd, &["push", "origin", ":feat"]);
    assert!(
        !r.ok && r.stderr.contains("remote ref does not exist"),
        "{}",
        mask(&r.stderr)
    );
    assert_eq!(srv.user_refs(), before);

    // A shared tag.
    git.ok(&a, &["checkout", "-q", "master"]);
    git.ok(&a, &["tag", "-a", "v1", "-m", "one"]);
    assert!(git.ssh(&a, &acmd, &["push", "origin", "v1"]).ok);
}

#[test]
#[ignore = "TODO(C34): AGit refs/for needs Forgejo's proc-receive; the standalone layer refuses it"]
fn agit_refs_for_opens_a_draft_change_over_ssh() {
    let tools = need_tools!();
    let srv = InProc::start(quiet(), "");
    let git = Git::new();
    let keys = tempfile::tempdir().unwrap();
    let alice = tools.key(keys.path(), "alice", "ed25519");
    srv.fake
        .add(&alice, user_row(1, (7, "alice"), &[(SOLO, "write")]));
    let cmd = srv.cmd(&tools, &alice);
    let a = git.repo_with_commits("a", 1);
    git.ok(&a, &["remote", "add", "origin", &srv.url(SOLO)]);
    assert!(git.ssh(&a, &cmd, &["push", "origin", "master"]).ok);
    git.ok(&a, &["checkout", "-q", "-b", "feat"]);
    git.commit(&a, "feat.txt", "f\n", "feat");
    let before = srv.changes();
    let r = git.ssh(&a, &cmd, &["push", "origin", "HEAD:refs/for/master"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert!(srv.changes() > before);
}

// =================================================================================== mirror, audit, limits

#[test]
fn a_mirror_mode_repository_is_read_only_over_ssh() {
    let tools = need_tools!();
    let srv = InProc::start(quiet(), "");
    let git = Git::new();
    let keys = tempfile::tempdir().unwrap();
    let alice = tools.key(keys.path(), "alice", "ed25519");
    srv.fake
        .add(&alice, user_row(1, (7, "alice"), &[(MIRROR, "write")]));
    let cmd = srv.cmd(&tools, &alice);
    let a = git.repo_with_commits("a", 1);
    let r = git.ssh(&a, &cmd, &["push", &srv.url(MIRROR), "master"]);
    assert!(!r.ok, "a mirror repository takes no push");
    assert!(r.stderr.contains("mirror"), "{}", mask(&r.stderr));
    let r = git.ssh(&a, &cmd, &["ls-remote", &srv.url(MIRROR)]);
    // Reads of a mirror that was never imported are the import unit's business: either the
    // empty answer or an `asc:` line, never a crash of the listener.
    assert!(r.ok || r.stderr.contains("asc:"), "{}", mask(&r.stderr));
}

#[test]
fn each_login_and_command_writes_one_audit_record_without_key_material() {
    let tools = need_tools!();
    let srv = InProc::start(quiet(), "");
    let git = Git::new();
    let keys = tempfile::tempdir().unwrap();
    let alice = tools.key(keys.path(), "alice", "ed25519");
    let ghost = tools.key(keys.path(), "ghost", "ecdsa");
    srv.fake
        .add(&alice, user_row(1, (7, "alice"), &[(WEB, "write")]));
    let cmd = srv.cmd(&tools, &alice);

    let a = git.repo_with_commits("a", 1);
    assert!(
        git.ssh(&a, &cmd, &["push", "-q", &srv.url(WEB), "master"])
            .ok
    );
    assert!(git.ssh(&a, &cmd, &["ls-remote", &srv.url(WEB)]).ok);
    let r = tools.run_ssh(
        git.home.path(),
        srv.ssh.port(),
        Some(&alice),
        &srv.known_hosts,
        &["git@127.0.0.1", "id"],
    );
    assert_eq!(r.code, Some(1));
    let _ = git.ssh(&a, &srv.cmd(&tools, &ghost), &["ls-remote", &srv.url(WEB)]);
    std::thread::sleep(Duration::from_millis(500));

    let rec = &srv.audit;
    let ok_logins = rec
        .logins
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.principal.is_some())
        .count();
    assert_eq!(
        ok_logins,
        3,
        "one record per successful login: {}",
        rec.dump()
    );
    assert!(
        rec.logins
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.principal.is_none()),
        "the refused login is recorded too"
    );
    let git_records = rec.git.lock().unwrap();
    assert_eq!(
        git_records.len(),
        2,
        "one record per pack command: {git_records:?}"
    );
    assert!(
        git_records
            .iter()
            .all(|g| g.transport == "ssh" && g.repo == WEB)
    );
    drop(git_records);
    assert!(
        !rec.refused.lock().unwrap().is_empty(),
        "the refused `id` is audited"
    );

    let dump = rec.dump();
    for key in [&alice, &ghost] {
        assert!(
            !dump.contains(&key.blob()),
            "a public key blob in an audit record"
        );
        assert!(
            !dump.contains(&key.private_body()),
            "private key bytes in an audit record"
        );
    }
    assert!(
        dump.contains("SHA256:"),
        "records name a key by its fingerprint"
    );
    let _ = &srv.dir;
}

#[test]
fn admission_sheds_over_limit_logins_and_fetches() {
    let tools = need_tools!();
    let limits = SshLimitsConfig {
        max_unauthenticated: 1,
        max_unauthenticated_per_peer: 1,
        ..quiet()
    };
    let srv = InProc::start(limits, "fetch_per_principal = 1\n");
    let git = Git::new();
    let keys = tempfile::tempdir().unwrap();
    let alice = tools.key(keys.path(), "alice", "ed25519");
    srv.fake
        .add(&alice, user_row(1, (7, "alice"), &[(WEB, "write")]));
    let cmd = srv.cmd(&tools, &alice);
    let a = git.repo_with_commits("a", 1);
    assert!(
        git.ssh(&a, &cmd, &["push", "-q", &srv.url(WEB), "master"])
            .ok
    );

    // One connection that never speaks holds the only unauthenticated slot.
    let hog = std::net::TcpStream::connect(srv.ssh).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let r = git.ssh(&a, &cmd, &["ls-remote", &srv.url(WEB)]);
    assert!(!r.ok, "the over-limit login is shed: {}", r.stdout);
    drop(hog);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        git.ssh(&a, &cmd, &["ls-remote", &srv.url(WEB)]).ok,
        "and served again after"
    );

    // A fetch that holds its slot (the client never answers the advertisement) sheds the next.
    let mut holder = Command::new(&tools.ssh);
    holder
        .args(["-p", &srv.ssh.port().to_string(), "-i"])
        .arg(&alice.path)
        .args(["-o", "IdentitiesOnly=yes"])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", fwd(&srv.known_hosts)))
        .args(["git@127.0.0.1", "git-upload-pack 'acme/web.git'"])
        .env("HOME", git.home.path())
        .env("USERPROFILE", git.home.path())
        .env_remove("SSH_AUTH_SOCK")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut held = holder.spawn().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let r = git.ssh(&a, &cmd, &["ls-remote", &srv.url(WEB)]);
    let _ = held.kill();
    let _ = held.wait();
    assert!(!r.ok, "the over-limit fetch is shed");
    assert!(
        r.stderr.contains("too many concurrent operations"),
        "{}",
        mask(&r.stderr)
    );
}
