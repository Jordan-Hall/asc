//! The branchless cases of SPEC §25.6 through real git and the real `asc serve`: a plain push of
//! a non-shared branch becomes the pusher's user ref (and opens no change), `refs/for/<trunk>`
//! opens a draft change, a push to the trunk follows the token's rights, a deletion removes the
//! user ref, and the pusher's user refs are not advertised to a second principal.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::{RepoName, ServerInfo};
use asc_serve::config::ServeConfig;
use asc_serve::git_push::HookProgram;
use asc_serve::git_view::GitViewService;
use asc_serve::http::Metrics;
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::registry::{Extensions, NewRepo, Registry};
use asc_serve::{git_http, migrate};
use sqlx::Row;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

use crate::harness::*;

const ALICE: &str = "tok-alice-0123456789abcdef";
const CAROL: &str = "tok-carol-0123456789abcdef";
const BOB: &str = "tok-bob-0123456789abcdef";

/// Forgejo-shaped principals (`<user_id>:<login>`): only a verified Forgejo identity owns user
/// refs (SPEC §15.1, B08a), and the token store issues none, so this case runs the production
/// git wiring in process with an authorizer that returns them.
struct Acl;

#[async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let token = token.ok_or(AuthError::Unauthenticated)?;
        let (id, max) = match token.expose() {
            ALICE => ("7:alice", Access::Write),
            CAROL => ("8:carol", Access::Write),
            BOB => ("9:bob", Access::Read),
            _ => return Err(AuthError::Unauthenticated),
        };
        if repo == "acme/web" && access <= max {
            Ok(Principal { id: id.to_owned() })
        } else {
            Err(AuthError::Forbidden)
        }
    }
}

/// The git layer as `asc serve` wires it: the view service, the routes, the credential edge and
/// admission, the pre-receive hook running the built `asc` binary. Dropping it stops the thread.
struct InProc {
    addr: SocketAddr,
    registry: Arc<Registry>,
    handle: tokio::runtime::Handle,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _dir: tempfile::TempDir,
}

impl InProc {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let path = dir.path().to_owned();
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
                     [limits.git]\nmin_free_disk = \"1KiB\"\n",
                    fwd(&path.join("cache"))
                ))
                .unwrap();
                let pool = migrate::connect(&db, 4).await.unwrap();
                migrate::migrate(&pool).await.unwrap();
                let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
                registry
                    .create(&RepoName::parse("acme/web").unwrap(), &NewRepo::default())
                    .await
                    .unwrap();
                sqlx::query("UPDATE repos SET branchless = TRUE WHERE name = $1")
                    .bind("acme/web")
                    .execute(registry.pool())
                    .await
                    .unwrap();
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
                git_http::register_with_hook(&ext, views, registry.clone(), &metrics, &cfg, &hook);
                let admission = Arc::new(Admission::new(&cfg.limits));
                let routes = guarded_routes(&ext, Arc::new(Acl), admission, ServerInfo::default());
                let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                tx.send((incoming.local_addr().unwrap(), registry, handle))
                    .unwrap();
                let _ = Server::builder()
                    .accept_http1(true)
                    .add_routes(routes)
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        let (addr, registry, handle) = rx.recv().unwrap();
        Self {
            addr,
            registry,
            handle,
            stop: Some(stop_tx),
            _dir: dir,
        }
    }

    fn url(&self, token: &str) -> String {
        format!("http://x-access-token:{token}@{}/acme/web.git", self.addr)
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

fn commit(git: &Git, dir: &Path, file: &str, body: &str, msg: &str) {
    std::fs::write(dir.join(file), body).unwrap();
    git.ok(dir, &["add", "."]);
    git.ok(dir, &["commit", "-q", "-m", msg]);
}

#[test]
fn plain_pushes_follow_the_branchless_rules() {
    let stack = InProc::start();
    let git = Git::new();
    let url = stack.url(ALICE);
    let a = git.repo_with_commits("a", 1);
    git.ok(&a, &["remote", "add", "origin", &url]);

    // The trunk is a shared ref: a write token pushes it, a read token is refused.
    let r = git.run(&a, &["push", "origin", "master"]);
    assert!(r.ok, "trunk push: {}", mask(&r.stderr));
    let r = git.run(&a, &["push", &stack.url(BOB), "HEAD:refs/heads/master"]);
    assert!(!r.ok, "a read token does not push the trunk");

    // `git push origin HEAD` on a non-shared branch: a user ref, the line, no change.
    git.ok(&a, &["checkout", "-q", "-b", "feat"]);
    commit(&git, &a, "feat.txt", "f\n", "feat");
    let (users, changes_before) = (stack.user_refs(), stack.changes());
    let r = git.run(&a, &["push", "origin", "HEAD"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert!(
        r.stderr
            .contains("remote: asc: stored as your branch 'feat'"),
        "{}",
        mask(&r.stderr)
    );
    assert_eq!(stack.user_refs(), users + 1, "one user ref");
    assert_eq!(
        stack.changes(),
        changes_before,
        "no change (no PR) from a plain push"
    );

    // The pusher's user ref is not advertised: not to them, not to a second principal.
    let carol_url = stack.url(CAROL);
    for u in [&url, &carol_url] {
        let ls = git.ok(&git.work("ls"), &["ls-remote", u]);
        assert!(!ls.contains("feat"), "a user ref is not advertised: {ls}");
        assert!(!ls.contains("refs/users"), "{ls}");
        assert!(ls.contains("refs/heads/master"), "{ls}");
    }
    // A second principal pushing the same branch name gets a user ref of their own.
    let c = git.work("c");
    git.ok(&c, &["clone", "-q", &carol_url, "w"]);
    let cw = c.join("w");
    git.ok(&cw, &["checkout", "-q", "-b", "feat"]);
    commit(&git, &cw, "carol.txt", "c\n", "carol feat");
    let r = git.run(&cw, &["push", "origin", "HEAD"]);
    assert!(r.ok, "same name, other owner: {}", mask(&r.stderr));
    assert_eq!(stack.user_refs(), users + 2);

    // `HEAD:refs/for/<trunk>` is AGit: the forge's proc-receive opens the draft change (C34, a
    // Forgejo unit). Without the forge the layer refuses it with a clear reason and creates
    // neither a ref nor a change; `agit_refs_for_opens_a_draft_change` is the SPEC behaviour.
    let r = git.run(&a, &["push", "origin", "HEAD:refs/for/master"]);
    assert!(!r.ok, "refs/for without the forge is refused");
    assert!(
        r.stderr
            .contains("remote: asc: refs/for/master is not a shared ref"),
        "{}",
        mask(&r.stderr)
    );
    assert_eq!(stack.changes(), changes_before, "no change was created");
    let ls = git.ok(&git.work("ls"), &["ls-remote", &url]);
    assert!(!ls.contains("refs/for"), "{ls}");

    // A deletion: user refs are not advertised, so git itself refuses `:feat` before it talks
    // to the server ("remote ref does not exist") and the user ref stays. SPEC §25.6 says the
    // deletion removes it; that needs the per-principal advertisement the SPEC leaves to v1.x,
    // so `deleting_a_user_ref_through_git` is the SPEC behaviour, ignored until then.
    let before = stack.user_refs();
    let r = git.run(&a, &["push", "origin", ":feat"]);
    assert!(
        !r.ok && r.stderr.contains("remote ref does not exist"),
        "{}",
        mask(&r.stderr)
    );
    assert_eq!(stack.user_refs(), before, "nothing was deleted");

    // An annotated tag of the trunk is a shared ref.
    git.ok(&a, &["checkout", "-q", "master"]);
    git.ok(&a, &["tag", "-a", "v1", "-m", "one"]);
    let r = git.run(&a, &["push", "origin", "v1"]);
    assert!(r.ok, "shared tag: {}", mask(&r.stderr));
    let ls = git.ok(&git.work("ls"), &["ls-remote", &url]);
    assert!(ls.contains("refs/tags/v1"), "{ls}");
}

/// SPEC §25.6: `git push origin HEAD:refs/for/<trunk>` creates one draft change per commit.
/// Needs the forge's proc-receive.
#[test]
#[ignore = "TODO(C34): AGit refs/for needs Forgejo's proc-receive; the standalone layer refuses it"]
fn agit_refs_for_opens_a_draft_change() {
    let stack = InProc::start();
    let git = Git::new();
    let a = git.repo_with_commits("a", 1);
    git.ok(&a, &["remote", "add", "origin", &stack.url(ALICE)]);
    assert!(git.run(&a, &["push", "origin", "master"]).ok);
    git.ok(&a, &["checkout", "-q", "-b", "feat"]);
    commit(&git, &a, "feat.txt", "f\n", "feat");
    let before = stack.changes();
    let r = git.run(&a, &["push", "origin", "HEAD:refs/for/master"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert!(stack.changes() > before, "a draft change was created");
}

/// SPEC §25.6: `git push origin :feat` deletes the pusher's user ref.
#[test]
#[ignore = "TODO(per-principal advertisement, SPEC 25.6 v1.x): git cannot name a ref it is not shown"]
fn deleting_a_user_ref_through_git() {
    let stack = InProc::start();
    let git = Git::new();
    let a = git.repo_with_commits("a", 1);
    git.ok(&a, &["remote", "add", "origin", &stack.url(ALICE)]);
    assert!(git.run(&a, &["push", "origin", "master"]).ok);
    git.ok(&a, &["checkout", "-q", "-b", "feat"]);
    commit(&git, &a, "feat.txt", "f\n", "feat");
    assert!(git.run(&a, &["push", "origin", "HEAD"]).ok);
    let before = stack.user_refs();
    let r = git.run(&a, &["push", "origin", ":feat"]);
    assert!(r.ok, "{}", mask(&r.stderr));
    assert_eq!(stack.user_refs(), before - 1);
}
