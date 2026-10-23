//! B06 acceptance: `asc gc` removes the un-uploaded objects only expired hidden drafts reach
//! (SPEC §21.2.4, local column), and nothing else, driven through the built binary and read
//! back from `store/` and `state.db`.

mod support;

use std::fs;
use std::path::PathBuf;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid};
use asc_state::state::{git_map, repos, workspaces};
use asc_state::{AscHome, StateDb};
use asc_store::cachedir::CacheDir;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::RepoKey;
use support::{Sandbox, text};

const DAY: i64 = 86_400;
const GONE: &str = "only the hidden draft has this\n";
const SHARED: &str = "shared bytes\n";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Repo {
    sb: Sandbox,
    root: PathBuf,
    /// The visible draft head and the hidden one.
    visible: ChangesetId,
    hidden: ChangesetId,
    hidden_git: String,
}

impl Repo {
    /// `base`, then `visible` (keep.txt), then `hidden` (gone.txt and a copy of keep.txt's
    /// bytes, which `visible` shares); the work tree goes back to `visible`. The draft rows are
    /// set by hand: `visible` is a visible head and `hidden` was hidden `hidden_ago` ago.
    fn new(hidden_ago: i64) -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let mut r = Self {
            sb,
            root,
            visible: ChangesetId::from_bytes([0; 32]),
            hidden: ChangesetId::from_bytes([0; 32]),
            hidden_git: String::new(),
        };
        r.ok(&["init"]);
        r.write("a.txt", "one\n");
        r.ok(&["add", "a.txt"]);
        r.ok(&["commit", "-m", "base"]);
        r.write("keep.txt", SHARED);
        r.ok(&["add", "keep.txt"]);
        r.ok(&["commit", "-m", "visible"]);
        r.visible = r.head_cs();
        r.write("gone.txt", GONE);
        r.write("copy.txt", SHARED);
        r.ok(&["add", "gone.txt", "copy.txt"]);
        r.ok(&["commit", "-m", "hidden"]);
        r.hidden = r.head_cs();
        r.hidden_git = r.ok(&["rev-parse", "HEAD"]).trim().to_owned();
        r.ok(&["reset", "--hard", "HEAD~1"]);
        r.set_rows(hidden_ago);
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

    fn with_db<T>(&self, f: impl AsyncFnOnce(&StateDb, RepoKey, &AscHome) -> T) -> T {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let repo = repos::get(&db, ws.ws.repo).await.unwrap().unwrap().key;
            f(&db, repo, &home).await
        })
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

    /// Replaces the draft rows with exactly one visible and one hidden head.
    fn set_rows(&self, hidden_ago: i64) {
        let (v, h) = (self.visible, self.hidden);
        self.with_db(async |db, repo, _| {
            for table in ["draft_heads", "cloud_queue"] {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE repo_id = ?"
                )))
                .bind(repo.0)
                .execute(db.pool())
                .await
                .unwrap();
            }
            for (cs, hidden_at) in [(v, None), (h, Some(now() - hidden_ago))] {
                sqlx::query(
                    "INSERT INTO draft_heads (repo_id, cs, change, added, hidden_at) \
                     VALUES (?, ?, ?, 1, ?)",
                )
                .bind(repo.0)
                .bind(cs.as_bytes().to_vec())
                .bind(vec![7u8; 16])
                .bind(hidden_at)
                .execute(db.pool())
                .await
                .unwrap();
            }
        });
    }

    fn head_rows(&self) -> i64 {
        self.with_db(async |db, repo, _| {
            sqlx::query_scalar("SELECT COUNT(*) FROM draft_heads WHERE repo_id = ?")
                .bind(repo.0)
                .fetch_one(db.pool())
                .await
                .unwrap()
        })
    }

    /// Which of `keys` `store/` holds.
    fn in_store(&self, keys: &[ObjKey]) -> Vec<bool> {
        self.with_db(async |_, _, home| {
            let store = CacheDir::open_read_only(home.store()).unwrap();
            store.has_many(keys).await.unwrap()
        })
    }
}

fn file(body: &str) -> ObjKey {
    ObjKey::File(FileId::of(body.as_bytes()))
}

#[test]
fn expired_hidden_draft_loses_only_its_own_objects() {
    let r = Repo::new(91 * DAY);
    let keys = [
        ObjKey::Changeset(r.hidden),
        file(GONE),
        ObjKey::Changeset(r.visible),
        file(SHARED),
        file("one\n"),
    ];
    assert_eq!(r.in_store(&keys), vec![true; 5]);
    let out = r.ok(&["gc"]);
    assert!(out.contains("drafts: removed"), "{out}");
    assert_eq!(
        r.in_store(&keys),
        vec![false, false, true, true, true],
        "the hidden draft's own changeset and file go; shared and visible objects stay"
    );
    assert_eq!(r.head_rows(), 1, "the expired row is dropped");
    assert_eq!(fs::read_to_string(r.root.join("keep.txt")).unwrap(), SHARED);
    r.ok(&["status"]);
    r.ok(&["log", "--oneline"]);
    // A second run has nothing left to do.
    r.ok(&["gc"]);
}

#[test]
fn hidden_draft_inside_its_retention_is_kept() {
    let r = Repo::new(89 * DAY);
    let keys = [ObjKey::Changeset(r.hidden), file(GONE)];
    r.ok(&["gc"]);
    assert_eq!(r.in_store(&keys), vec![true, true]);
    assert_eq!(r.head_rows(), 2);
}

#[test]
fn retention_follows_asc_draft_hidden_days() {
    let r = Repo::new(10 * DAY);
    let keys = [ObjKey::Changeset(r.hidden)];
    r.ok(&["gc"]);
    assert_eq!(r.in_store(&keys), vec![true], "default 90 days keeps it");
    r.ok(&["config", "asc.draft.hiddenDays", "5"]);
    r.ok(&["gc"]);
    assert_eq!(r.in_store(&keys), vec![false], "5 days drops it");
}

#[test]
fn dry_run_changes_nothing() {
    let r = Repo::new(91 * DAY);
    let keys = [ObjKey::Changeset(r.hidden), file(GONE)];
    let out = r.ok(&["gc", "--dry-run"]);
    assert!(out.contains("would remove"), "{out}");
    assert_eq!(r.in_store(&keys), vec![true, true]);
    assert_eq!(r.head_rows(), 2);
}

#[test]
fn objects_of_visible_heads_are_never_removed() {
    // Both heads visible and ancient: nothing is expired, so nothing goes.
    let r = Repo::new(91 * DAY);
    r.with_db(async |db, repo, _| {
        sqlx::query("UPDATE draft_heads SET hidden_at = NULL WHERE repo_id = ?")
            .bind(repo.0)
            .execute(db.pool())
            .await
            .unwrap();
    });
    let keys = [
        ObjKey::Changeset(r.hidden),
        ObjKey::Changeset(r.visible),
        file(GONE),
    ];
    r.ok(&["gc"]);
    assert_eq!(r.in_store(&keys), vec![true; 3]);
}

#[test]
fn an_expired_head_that_a_ref_reaches_keeps_its_objects() {
    let r = Repo::new(91 * DAY);
    r.ok(&["branch", "keep-hidden", &r.hidden_git]);
    let keys = [ObjKey::Changeset(r.hidden), file(GONE)];
    r.ok(&["gc"]);
    assert_eq!(r.in_store(&keys), vec![true, true]);
}
