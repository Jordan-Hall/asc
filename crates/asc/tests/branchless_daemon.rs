//! B14 acceptance (SPEC §21.3.3), through the built binary: a commit joins the upload queue and
//! never contacts the server itself; with no daemon (`ASC_NO_DAEMON`) nothing is uploaded in the
//! background; an unreachable server never blocks or prints anything. The daemon's own policy
//! (gate, backoff, offline probe, bounds, the head sync) is tested in `daemon::cloud::tests`
//! against fakes and a real `state.db`.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use asc_state::state::workspaces;
use asc_state::{AscHome, StateDb};
use server_fixture::{Server, WRITE};
use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

/// The RPCs that move drafts to the server.
const UPLOAD_RPCS: [&str; 4] = ["PutObjects", "Ingest", "UpdateWorkspace", "GetWorkspace"];

struct Client {
    sb: Sandbox,
    root: PathBuf,
}

impl Client {
    fn new(url: &str) -> Self {
        let sb = Sandbox::new();
        let root = sb.dir("repo");
        let c = Self { sb, root };
        c.ok(&["init", "-q"]);
        c.ok(&["remote", "add", "origin", url]);
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.sb
            .asc(&self.root)
            .envs(IDENT)
            .env("ASC_TOKEN", WRITE)
            // No daemon: this is the CI and test path of SPEC §21.3.3.
            .env("ASC_NO_DAEMON", "1")
            .stdin(std::process::Stdio::null())
            .args(args)
            .output()
            .expect("spawn asc")
    }

    fn ok(&self, args: &[&str]) -> std::process::Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "asc {args:?} failed: {}",
            text(&out.stderr)
        );
        out
    }

    fn commit(&self, name: &str) -> std::process::Output {
        std::fs::write(self.root.join(name), format!("{name}\n")).unwrap();
        self.ok(&["add", name]);
        self.ok(&["commit", "-q", "-m", name])
    }

    /// `(pending queue rows, draft heads)` of the repository.
    fn rows(&self) -> (i64, i64) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let count = |table: &'static str| {
                let db = db.clone();
                async move {
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                        "SELECT COUNT(*) FROM {table} WHERE repo_id = ?"
                    )))
                    .bind(ws.ws.repo.0)
                    .fetch_one(db.pool())
                    .await
                    .unwrap()
                }
            };
            (count("cloud_queue").await, count("draft_heads").await)
        })
    }
}

fn uploads(server: &Server) -> Vec<String> {
    server
        .calls()
        .into_iter()
        .filter(|p| UPLOAD_RPCS.iter().any(|r| p.ends_with(&format!("/{r}"))))
        .collect()
}

#[test]
fn without_a_daemon_a_commit_is_queued_and_nothing_is_uploaded() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    server.forget_calls();

    c.commit("one.txt");
    c.commit("two.txt");

    let (queued, heads) = c.rows();
    assert!(queued >= 1, "the new head is queued ({queued} rows)");
    assert!(heads >= 1, "the draft head is recorded ({heads} rows)");
    assert_eq!(
        uploads(&server),
        Vec::<String>::new(),
        "no upload without a daemon; calls: {:?}",
        server.calls()
    );
}

#[test]
fn an_unreachable_server_never_blocks_or_prints_anything() {
    // Nothing listens on port 1.
    let c = Client::new("asc+http://127.0.0.1:1/acme/app");
    let started = Instant::now();
    let out = c.commit("one.txt");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "an offline commit is not slowed by the queue"
    );
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr), "");
    let (queued, _) = c.rows();
    assert!(queued >= 1, "the commit waits in the queue for the daemon");
}
