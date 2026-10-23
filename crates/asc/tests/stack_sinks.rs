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
use asc_state::oplog::{self, Op};
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

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root.join(name)).unwrap()
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

    /// The op log, oldest first.
    fn ops(&self) -> Vec<Op> {
        let mut ops = self.with_db(async |db, repo, _| oplog::list(db, repo, 100).await.unwrap());
        ops.reverse();
        ops
    }

    fn last_op(&self, kind: &str) -> Op {
        self.ops()
            .into_iter()
            .rev()
            .find(|o| o.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} op in the log"))
    }

    /// The git blob id of `path` in the changeset `cs`.
    fn blob_in(&self, cs: ChangesetId, path: &str) -> GitOid {
        self.with_db(async |_, _, engine| {
            let root = engine.get_changeset(cs).await.unwrap().root;
            let entry = engine
                .lookup(root, &asc_core::path::RepoPath::new(path).unwrap())
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{path} is not in the snapshot"));
            entry.git
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

    /// The blob id git would give `content`, taken from a stash of that content.
    fn blob_of_dirty(&self, path: &str) -> GitOid {
        let w = self.ok(&["stash", "create"]);
        self.ok(&["rev-parse", &format!("{}:{path}", w.trim())])
            .trim()
            .parse()
            .unwrap()
    }
}

#[test]
fn reset_hard_saves_the_dirty_work_in_a_snapshot() {
    let r = Repo::new();
    r.write("a.txt", "dirty\n");
    let want = r.blob_of_dirty("a.txt");
    let before = r.head_cs();
    r.ok(&["reset", "--hard"]);
    assert_eq!(r.read("a.txt"), "one\n");
    let op = r.last_op("reset");
    assert_eq!(op.argv, "reset --hard");
    let snap = op.wc_snapshot.expect("a snapshot");
    assert_eq!(op.head_before, Some(before));
    assert_eq!(op.head_after, Some(before));
    assert_eq!(r.blob_in(snap, "a.txt"), want);
}

#[test]
fn checkout_force_and_switch_discard_changes_leave_snapshots() {
    let r = Repo::new();
    r.write("a.txt", "dirty one\n");
    let want = r.blob_of_dirty("a.txt");
    r.ok(&["checkout", "-f", "master"]);
    assert_eq!(r.read("a.txt"), "one\n");
    let snap = r.last_op("checkout").wc_snapshot.expect("checkout -f");
    assert_eq!(r.blob_in(snap, "a.txt"), want);

    r.ok(&["branch", "other"]);
    r.write("a.txt", "dirty two\n");
    let want = r.blob_of_dirty("a.txt");
    r.ok(&["switch", "--discard-changes", "other"]);
    assert_eq!(r.read("a.txt"), "one\n");
    let snap = r.last_op("switch").wc_snapshot.expect("switch");
    assert_eq!(r.blob_in(snap, "a.txt"), want);
}

#[test]
fn staged_work_is_in_the_snapshot_too() {
    let r = Repo::new();
    r.write("b.txt", "staged\n");
    r.ok(&["add", "b.txt"]);
    r.ok(&["reset", "--hard"]);
    let snap = r.last_op("reset").wc_snapshot.expect("a snapshot");
    // The index state is the second parent, W's tree holds the staged file on top of HEAD.
    let _ = r.blob_in(snap, "b.txt");
}

#[test]
fn a_clean_tree_needs_no_snapshot_and_read_only_commands_leave_no_row() {
    let r = Repo::new();
    let rows = r.ops().len();
    r.ok(&["status"]);
    r.ok(&["log", "--oneline"]);
    r.ok(&["diff"]);
    assert_eq!(r.ops().len(), rows);
    // `reset --hard` moves HEAD's reflog even here, but has nothing to save.
    r.ok(&["reset", "--hard"]);
    assert!(r.last_op("reset").wc_snapshot.is_none());
}

#[test]
fn commands_that_move_refs_are_one_op_each_with_the_shared_op_id() {
    let r = Repo::new();
    let first = r.head_cs();
    r.write("a.txt", "two\n");
    r.ok(&["commit", "-a", "-m", "two"]);
    let op = r.last_op("commit");
    assert_eq!(op.argv, "commit -a -m two");
    assert_eq!(op.head_before, Some(first));
    assert_eq!(op.head_after, Some(r.head_cs()));
    assert!(op.wc_snapshot.is_none());
    // The ref_log rows of the command carry the op's id.
    let moved: i64 = r.with_db(async |db, repo, _| {
        sqlx::query_scalar("SELECT COUNT(*) FROM ref_log WHERE repo_id = ? AND op_id = ?")
            .bind(repo.0)
            .bind(op.op_id.0.as_bytes().to_vec())
            .fetch_one(db.pool())
            .await
            .unwrap()
    });
    assert!(moved >= 1);
}

#[test]
fn commit_and_amend_feed_the_change_index() {
    let r = Repo::new();
    let base = r.ok(&["change-id", "HEAD"]).trim().to_owned();
    r.write("a.txt", "two\n");
    r.ok(&["commit", "-a", "-m", "two"]);
    let two = r.ok(&["change-id", "HEAD"]).trim().to_owned();
    assert_ne!(base, two);
    r.write("a.txt", "three\n");
    r.ok(&["commit", "-a", "--amend", "-m", "two again"]);
    assert_eq!(r.ok(&["change-id", "HEAD"]).trim(), two);
    // Rebasing keeps the id as well.
    r.ok(&["switch", "-c", "side", "HEAD~1"]);
    r.write("s.txt", "side\n");
    r.ok(&["add", "s.txt"]);
    r.ok(&["commit", "-m", "side"]);
    let side = r.ok(&["change-id", "HEAD"]).trim().to_owned();
    r.ok(&["rebase", "master"]);
    assert_eq!(r.ok(&["change-id", "HEAD"]).trim(), side);
}

#[test]
fn clean_leaves_a_row_without_a_snapshot() {
    let r = Repo::new();
    r.write("scratch.txt", "x\n");
    r.ok(&["clean", "-f"]);
    assert!(!r.root.join("scratch.txt").exists());
    let op = r.last_op("clean");
    assert_eq!(op.argv, "clean -f");
    assert!(op.wc_snapshot.is_none());
    // A dry run destroys nothing and leaves no row.
    r.write("scratch.txt", "x\n");
    let rows = r.ops().len();
    r.ok(&["clean", "-n"]);
    assert_eq!(r.ops().len(), rows);
}

#[test]
fn the_op_log_can_be_switched_off_and_the_change_index_stays() {
    let r = Repo::new();
    r.ok(&["config", "asc.opLog", "false"]);
    let rows = r.ops().len();
    r.write("a.txt", "dirty\n");
    r.ok(&["reset", "--hard"]);
    r.write("a.txt", "two\n");
    r.ok(&["commit", "-a", "-m", "two"]);
    assert_eq!(r.ops().len(), rows);
    assert!(r.run(&["change-id", "HEAD"]).0 == 0);
}

#[test]
fn the_op_log_is_bounded_by_max_ops() {
    let r = Repo::new();
    r.ok(&["config", "asc.opLog.maxOps", "3"]);
    for i in 0..6 {
        r.write("a.txt", &format!("v{i}\n"));
        r.ok(&["commit", "-a", "-m", &format!("c{i}")]);
    }
    assert_eq!(r.ops().len(), 3);
}

#[test]
fn a_failing_command_still_closes_its_op() {
    let r = Repo::new();
    let rows = r.ops().len();
    let (code, _, _) = r.run(&["reset", "--hard", "no-such-rev"]);
    assert_ne!(code, 0);
    assert_eq!(r.ops().len(), rows);
    // The next command gets a fresh op id.
    r.write("a.txt", "two\n");
    r.ok(&["commit", "-a", "-m", "two"]);
    assert_eq!(r.ops().len(), rows + 1);
}
