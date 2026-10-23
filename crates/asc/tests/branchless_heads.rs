//! C04 acceptance: the invisible op log, the change index and the pre-destroy snapshots that
//! `ext::rewrite_sink()` and `ext::op_sink()` fill (SPEC §20.3), driven through the built
//! binary and read back from `state.db`.

mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid};
use asc_engine::Engine;
use asc_state::drafts::Drafts;
use asc_state::state::{git_map, repos, workspaces};
use asc_state::{AscHome, StateDb};
use asc_store::cachedir::CacheDir;
use asc_store::refs::RepoKey;
use asc_store::storeset::StoreSet;
use support::{Sandbox, text};

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let r = Self { sb, root };
        r.ok(&["init"]);
        r.write("a.txt", "one\n");
        r.ok(&["add", "a.txt"]);
        r.ok(&["commit", "-m", "base"]);
        r
    }

    fn write(&self, name: &str, body: &str) {
        fs::write(self.root.join(name), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = self
            .sb
            .asc(&self.root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn");
        (
            out.status.code().unwrap_or(-1),
            text(&out.stdout),
            text(&out.stderr),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "asc {args:?}: {err}");
        out
    }

    /// Runs `f` against the repository's `state.db` and engine.
    fn with_db<T>(
        &self,
        f: impl AsyncFnOnce(&StateDb, RepoKey, &Engine<StoreSet, asc_store::sqlrefs::SqlRefs>) -> T,
    ) -> T {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let repo = repos::get(&db, ws.ws.repo).await.unwrap().unwrap().key;
            let objects = Arc::new(StoreSet {
                store: CacheDir::open(home.store(), home.tmp()).unwrap(),
                cache: CacheDir::open(home.cache(), home.tmp()).unwrap(),
            });
            let engine = Engine::new(objects, Arc::new(db.refs()), repo);
            f(&db, repo, &engine).await
        })
    }

    fn heads(&self) -> Vec<ChangesetId> {
        let mut v = self.with_db(async |db, repo, _| {
            Drafts::new(repo)
                .visible(db, 100)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.cs)
                .collect::<Vec<_>>()
        });
        v.sort();
        v
    }

    fn queued(&self) -> Vec<ChangesetId> {
        let mut v = self.with_db(async |db, repo, _| {
            Drafts::new(repo)
                .due(db, i64::MAX, 100)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.cs)
                .collect::<Vec<_>>()
        });
        v.sort();
        v
    }

    fn head_cs(&self) -> ChangesetId {
        let sha: GitOid = self.ok(&["rev-parse", "HEAD"]).trim().parse().unwrap();
        self.with_db(async |db, repo, _| {
            ChangesetId::from_bytes(
                git_map::git_to_asc(db, repo, sha, ObjectKind::Commit)
                    .await
                    .unwrap()
                    .unwrap(),
            )
        })
    }
}

fn check(mode: &str) {
    let r = Repo::new();
    r.ok(&["config", "asc.branchless", mode]);
    let base = r.head_cs();
    r.write(
        "a.txt", "two
",
    );
    r.ok(&["commit", "-am", "second"]);
    let second = r.head_cs();
    assert_eq!(r.heads(), vec![second], "a new commit replaces its parent");
    assert_eq!(r.queued(), vec![second]);
    assert!(!r.heads().contains(&base));

    r.write(
        "a.txt", "three
",
    );
    r.ok(&["commit", "--amend", "-am", "second again"]);
    let amended = r.head_cs();
    assert_ne!(amended, second);
    assert_eq!(r.heads(), vec![amended], "an amend replaces the old head");
    assert_eq!(r.queued(), vec![amended]);
}

#[test]
fn heads_and_queue_follow_commit_and_amend_with_branchless_on() {
    check("true");
}

#[test]
fn heads_and_queue_follow_commit_and_amend_with_branchless_off() {
    check("false");
}

#[test]
fn read_only_commands_leave_the_tables_alone() {
    let r = Repo::new();
    let (h, q) = (r.heads(), r.queued());
    r.ok(&["status"]);
    r.ok(&["log", "--oneline"]);
    assert_eq!((r.heads(), r.queued()), (h, q));
}
