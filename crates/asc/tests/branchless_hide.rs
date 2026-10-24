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
use asc_store::refs::{Refs as _, RepoKey};
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

impl Repo {
    fn hidden(&self) -> Vec<ChangesetId> {
        let mut v = self.with_db(async |db, repo, _| {
            Drafts::new(repo)
                .hidden(db, 100)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.cs)
                .collect::<Vec<_>>()
        });
        v.sort();
        v
    }

    fn sha(&self, rev: &str) -> String {
        self.ok(&["rev-parse", rev]).trim().to_owned()
    }

    fn cs(&self, rev: &str) -> ChangesetId {
        let sha: GitOid = self.sha(rev).parse().unwrap();
        self.with_db(async |db, repo, _| {
            ChangesetId::from_bytes(
                git_map::git_to_asc(db, repo, sha, ObjectKind::Commit)
                    .await
                    .unwrap()
                    .unwrap(),
            )
        })
    }

    /// `base` on master, then `a` and `b` on `feat`.
    fn stack() -> (Self, ChangesetId, ChangesetId) {
        let r = Self::new();
        r.ok(&["checkout", "-b", "feat"]);
        r.write(
            "a.txt", "a
",
        );
        r.ok(&["commit", "-am", "a"]);
        let a = r.head_cs();
        r.write(
            "a.txt", "b
",
        );
        r.ok(&["commit", "-am", "b"]);
        let b = r.head_cs();
        (r, a, b)
    }
}

#[test]
fn hide_a_mid_stack_change_hides_the_head_and_undo_restores_it() {
    let (r, _a, b) = Repo::stack();
    assert_eq!(r.heads(), vec![b]);
    let out = r.ok(&["hide", "feat~1"]);
    assert!(out.starts_with("Hid "), "{out}");
    assert!(r.heads().is_empty());
    assert_eq!(r.hidden(), vec![b]);
    assert!(
        r.queued().is_empty(),
        "a hidden head leaves the upload queue"
    );

    r.ok(&["undo"]);
    // The next hide or unhide reconciles the rows with the op log.
    r.ok(&["hide", "-n", "feat~1"]);
    assert_eq!(r.heads(), vec![b], "undo brings the head back");
    assert!(r.hidden().is_empty());
}

#[test]
fn hide_the_top_exposes_its_parent_and_undo_takes_it_back() {
    let (r, a, b) = Repo::stack();
    r.ok(&["hide", "feat"]);
    assert_eq!(r.heads(), vec![a], "the parent becomes the head");
    assert_eq!(r.hidden(), vec![b]);
    assert_eq!(r.queued(), vec![a]);

    r.ok(&["undo"]);
    r.ok(&["hide", "-n", "feat"]);
    assert_eq!(r.heads(), vec![b]);
    assert!(r.hidden().is_empty());
    assert!(!r.queued().contains(&a) || r.queued().contains(&b));
}

#[test]
fn unhide_recovers_a_hidden_draft_within_the_grace_period() {
    let (r, a, b) = Repo::stack();
    let bsha = "feat";
    r.ok(&["hide", "feat~1"]);
    assert!(r.heads().is_empty());
    let out = r.ok(&["unhide", bsha]);
    assert!(out.starts_with("Unhid "), "{out}");
    assert_eq!(r.heads(), vec![b]);
    assert!(r.hidden().is_empty());
    assert_eq!(r.queued(), vec![b], "unhide enqueues the head again");

    // Undo of the unhide hides it again.
    r.ok(&["undo"]);
    r.ok(&["unhide", "-n", bsha]);
    assert!(r.heads().is_empty());
    assert_eq!(r.hidden(), vec![b]);
    let _ = a;
}

#[test]
fn unhide_of_a_draft_that_is_no_head_inserts_it() {
    let (r, a, b) = Repo::stack();
    r.ok(&["unhide", "feat~1"]);
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(r.heads(), want);
    assert_eq!(r.cs("feat~1"), a);
}

#[test]
fn hide_refuses_a_change_that_is_not_a_draft_or_not_visible() {
    let (r, _a, _b) = Repo::stack();
    let (code, _, err) = r.run(&["hide", "master"]);
    assert_eq!(code, 1, "{err}");
    r.ok(&["hide", "feat~1"]);
    let (code, _, err) = r.run(&["hide", "feat"]);
    assert_eq!(code, 1, "hiding a hidden change: {err}");
    let (code, _, _) = r.run(&["hide"]);
    assert_ne!(code, 0);
}

#[test]
fn the_marker_refs_do_not_root_the_hidden_draft() {
    let (r, _a, b) = Repo::stack();
    r.ok(&["hide", "feat~1"]);
    let hidden_targets = r.with_db(async |db, repo, _| {
        db.refs()
            .list(repo, "refs/asc/hidden/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.target)
            .collect::<Vec<_>>()
    });
    assert_eq!(hidden_targets.len(), 1);
    assert_ne!(
        hidden_targets[0],
        asc_core::id::RefTarget::Changeset(b),
        "a marker on the draft would pin it past the retention"
    );
}
