//! B16 acceptance (SPEC §21.3): `asc cloud status|sync|join|list|release|purge`, through the built
//! binary. `status` reads `state.db` only; `sync`, `list`, `release` and `purge` report a server
//! that has no commit cloud instead of hiding it; `purge` deletes nothing without `--yes`; `join`
//! refuses a bad name and clears the sync base of the workspace it leaves. The server-facing
//! logic that needs a cloud service is covered by the unit tests in `stack::cli::cloud`.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::PathBuf;

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

    fn commit(&self, name: &str) {
        std::fs::write(self.root.join(name), format!("{name}\n")).unwrap();
        self.ok(&["add", name]);
        self.ok(&["commit", "-q", "-m", name]);
    }

    fn status_json(&self) -> serde_json::Value {
        let out = self.ok(&["cloud", "status", "--json"]);
        serde_json::from_str(text(&out.stdout).trim()).expect("one json line")
    }

    /// `(workspace name, version)` of the `cloud_state` row, if any.
    fn cloud_state(&self) -> Option<(String, i64)> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            asc_state::drafts::Drafts::new(
                asc_state::state::repos::get(&db, ws.ws.repo)
                    .await
                    .unwrap()
                    .unwrap()
                    .key,
            )
            .cloud_state(&db)
            .await
            .unwrap()
            .map(|s| (s.ws_name, s.version))
        })
    }

    fn set_cloud_state(&self, name: &str, version: i64) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let key = asc_state::state::repos::get(&db, ws.ws.repo)
                .await
                .unwrap()
                .unwrap()
                .key;
            asc_state::drafts::Drafts::new(key)
                .set_cloud_state(
                    &db,
                    &asc_state::drafts::CloudState {
                        ws_name: name.to_owned(),
                        version,
                        last_sync: 5,
                    },
                )
                .await
                .unwrap();
        });
    }
}

fn rpc_calls(server: &Server, name: &str) -> usize {
    server.count(name)
}

#[test]
fn status_reads_the_queue_without_the_network() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    let before = c.status_json();
    assert_eq!(before["state"], "synced");
    assert_eq!(before["queue"]["pending"], 0);
    assert_eq!(before["workspace"], "default");

    c.commit("one.txt");
    server.forget_calls();
    let after = c.status_json();
    assert_eq!(after["state"], "pending", "{after}");
    assert!(after["queue"]["pending"].as_i64().unwrap() >= 1);
    assert!(after["visible_heads"].as_i64().unwrap() >= 1);
    assert!(
        server.calls().is_empty(),
        "status contacted the server: {:?}",
        server.calls()
    );

    let text_out = text(&c.ok(&["cloud", "status"]).stdout);
    assert!(text_out.starts_with("cloud: "), "{text_out}");
    assert!(text_out.contains("queue:"), "{text_out}");
}

#[test]
fn status_says_off_when_the_user_turned_the_cloud_off() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    let out = c.ok(&["-c", "asc.cloud=off", "cloud", "status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert_eq!(v["state"], "off");
    assert_eq!(v["cloud"], "off");
}

#[test]
fn sync_reports_a_server_without_a_cloud_and_exits_non_zero() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    c.commit("one.txt");
    let out = c.run(&["cloud", "sync"]);
    assert!(
        !out.status.success(),
        "sync must fail loudly: {}",
        text(&out.stdout)
    );
    let err = text(&out.stderr);
    assert!(err.contains("no commit cloud"), "{err}");
    assert_eq!(rpc_calls(&server, "PutObjects"), 0, "nothing is uploaded");
    // The queue is untouched by the failure.
    assert!(c.status_json()["queue"]["pending"].as_i64().unwrap() >= 1);
}

#[test]
fn sync_with_the_cloud_off_uploads_nothing_and_says_why() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    c.commit("one.txt");
    server.forget_calls();
    let out = c.run(&["-c", "asc.cloud=off", "cloud", "sync"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("cloud is off"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(
        server.count("PutObjects") + server.count("UpdateWorkspace"),
        0
    );
}

#[test]
fn list_reports_a_server_without_a_cloud() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    let out = c.run(&["cloud", "list"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no commit cloud"),
        "{}",
        text(&out.stderr)
    );
}

/// A pushed base commit, then one draft change on a feature branch.
fn draft(c: &Client, name: &str) {
    std::fs::write(
        c.root.join("base.txt"),
        "base
",
    )
    .unwrap();
    c.ok(&["add", "base.txt"]);
    c.ok(&["commit", "-q", "-m", "base"]);
    c.ok(&["push", "origin", "master"]);
    c.ok(&["switch", "-q", "-c", "feature"]);
    std::fs::write(c.root.join(name), name).unwrap();
    c.ok(&["add", name]);
    c.ok(&["create", "-m", name]);
}

/// `(change id, changeset id)` of the draft at HEAD.
fn head_ids(c: &Client) -> (String, String) {
    let change = text(&c.ok(&["change-id", "HEAD"]).stdout).trim().to_owned();
    let cs = text(&c.ok(&["rev-parse", "--asc", "HEAD"]).stdout)
        .trim()
        .to_owned();
    (change, cs)
}

#[test]
fn purge_changes_nothing_without_yes() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    draft(&c, "secret.txt");
    let (change, _) = head_ids(&c);
    let queued = c.status_json()["queue"]["pending"].as_i64().unwrap();
    let heads = c.status_json()["visible_heads"].as_i64().unwrap();
    server.forget_calls();

    // A word that names no draft head is refused before anything is sent.
    let out = c.run(&["cloud", "purge", "ffffffff"]);
    assert!(!out.status.success());
    assert!(server.calls().is_empty(), "{:?}", server.calls());

    // A real change, without --yes: a plan, exit 1, no call, nothing hidden.
    let out = c.run(&["cloud", "purge", &change[..8]]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let plan = text(&out.stdout);
    assert!(plan.contains("would purge"), "{plan}");
    assert!(plan.contains("--yes"), "{plan}");
    assert!(server.calls().is_empty(), "{:?}", server.calls());
    let after = c.status_json();
    assert_eq!(after["queue"]["pending"].as_i64().unwrap(), queued);
    assert_eq!(after["visible_heads"].as_i64().unwrap(), heads);
}

#[test]
fn purge_with_yes_reports_a_server_without_a_cloud_and_keeps_the_heads() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    draft(&c, "secret.txt");
    let (change, _) = head_ids(&c);
    let heads = c.status_json()["visible_heads"].as_i64().unwrap();
    let out = c.run(&["cloud", "purge", "--yes", &change[..8]]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no commit cloud"),
        "{}",
        text(&out.stderr)
    );
    // The server did not purge, so the local heads are not hidden on its behalf.
    assert_eq!(c.status_json()["visible_heads"].as_i64().unwrap(), heads);
}

#[test]
fn join_refuses_a_bad_name_and_clears_the_base_of_the_old_workspace() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));

    for bad in ["bad name", "a/b", "", "x\u{1b}[31m"] {
        let out = c.run(&["cloud", "join", bad]);
        assert!(!out.status.success(), "{bad:?} must be refused");
    }
    assert_eq!(c.status_json()["workspace"], "default");

    c.set_cloud_state("default", 9);
    c.ok(&["cloud", "join", "laptop-2"]);
    assert_eq!(c.status_json()["workspace"], "laptop-2");
    // The version of the old workspace must not be read against the new one.
    assert_eq!(c.cloud_state(), Some(("laptop-2".to_owned(), 0)));

    // Joining the workspace already in use keeps the stored sync state.
    c.set_cloud_state("laptop-2", 4);
    c.ok(&["cloud", "join", "laptop-2"]);
    assert_eq!(c.cloud_state(), Some(("laptop-2".to_owned(), 4)));
}

#[test]
fn release_of_an_entry_that_is_not_waiting_is_refused() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    draft(&c, "one.txt");
    let (change, cs) = head_ids(&c);
    server.forget_calls();
    for word in [
        "ffffffff".to_owned(),
        change[..8].to_owned(),
        cs[..12].to_owned(),
    ] {
        let out = c.run(&["cloud", "release", &word]);
        assert!(!out.status.success(), "{word}");
    }
    // The entry is pending, not held: refused before any connection.
    assert!(server.calls().is_empty(), "{:?}", server.calls());
}

#[test]
fn sl_cloud_reports_a_server_without_a_cloud_before_printing_anything() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    c.commit("one.txt");
    for args in [
        &["sl", "--cloud"][..],
        &["sl", "--cloud", "--json"],
        &["sl", "--cloud", "--user", "bob", "-a"],
    ] {
        let out = c.run(args);
        assert!(!out.status.success(), "{args:?} must fail loudly");
        let err = text(&out.stderr);
        assert!(err.contains("no commit cloud"), "{args:?}: {err}");
        assert!(
            text(&out.stdout).is_empty(),
            "{args:?}: {}",
            text(&out.stdout)
        );
    }
}

#[test]
fn sl_user_needs_cloud_and_plain_sl_json_has_no_cloud_key() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    server.forget_calls();
    let out = c.run(&["sl", "--user", "bob"]);
    assert!(!out.status.success(), "--user alone must be refused");
    assert!(server.calls().is_empty(), "{:?}", server.calls());

    let out = c.ok(&["sl", "--json"]);
    let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert!(v.get("cloud").is_none(), "{v}");
    assert!(v["stacks"].is_array(), "{v}");
    assert!(
        server.calls().is_empty(),
        "plain sl never contacts the server"
    );
}
