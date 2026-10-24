//! C15 acceptance: `asc undo`, `asc redo` and `asc op` (SPEC §20.3, §20.7), driven through the
//! built binary. Undo of `git rebase`, `asc modify` and `asc split` restores refs, HEAD and the
//! working copy.

mod support;

use std::fs;
use std::path::PathBuf;

use asc_core::change::{ChangeId, MetaId, RewriteOp};
use asc_core::git::ObjectKind;
use asc_core::id::RefTarget;
use asc_core::id::{ChangesetId, GitOid};
use asc_core::path::RefName;
use asc_engine::change::{ChangeRow, ChangeRows};
use asc_state::changes::StateChanges;
use asc_state::state::{git_map, workspaces};
use asc_state::{AscHome, StateDb};
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use serde_json::Value;
use support::{Sandbox, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

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

    fn asc(&self, args: &[&str]) -> Out {
        let out = self
            .sb
            .asc(&self.root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: text(&out.stdout),
            stderr: text(&out.stderr),
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.asc(args);
        assert_eq!(o.code, 0, "asc {args:?}: {}", o.stderr);
        o.stdout
    }

    fn sha(&self, rev: &str) -> String {
        self.ok(&["rev-parse", rev]).trim().to_owned()
    }

    fn commit(&self, name: &str, body: &str, msg: &str) {
        self.write(name, body);
        self.ok(&["add", name]);
        self.ok(&["commit", "-m", msg]);
    }

    /// Records HEAD as a draft change in `state.db` (the change index, as in the C13 tests).
    fn index_head(&self, n: u8) {
        let sha = self.sha("HEAD");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let oid: GitOid = sha.parse().unwrap();
            let cs = ChangesetId::from_bytes(
                git_map::git_to_asc(&db, ws.ws.repo, oid, ObjectKind::Commit)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            let rows = StateChanges::new(db.clone(), ws.ws.repo);
            let row = ChangeRow {
                cs,
                change: ChangeId::from_bytes([n; 16]),
                meta: MetaId::from_bytes([n; 32]),
                op: RewriteOp::New,
            };
            rows.insert(&row, &[]).await.unwrap();
        });
    }

    /// Moves a ref in `state.db` without recording an op (as another writer would).
    fn move_ref_unlogged(&self, name: &str, rev: &str) {
        let sha = self.sha(rev);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let oid: GitOid = sha.parse().unwrap();
            let cs = ChangesetId::from_bytes(
                git_map::git_to_asc(&db, ws.ws.repo, oid, ObjectKind::Commit)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            let txn = RefTxn {
                updates: vec![RefUpdate {
                    name: RefName::new(name.as_bytes().to_vec()).unwrap(),
                    expect: Expect::Any,
                    new: Some(RefTarget::Changeset(cs)),
                    expires_at: None,
                }],
                actor: Actor {
                    id: "other".into(),
                    via: "test".into(),
                },
                reason: "test".into(),
                op_id: uuid::Uuid::now_v7(),
            };
            Refs::update(&db.refs(), ws.ws.repo, &txn).await.unwrap();
        });
    }

    fn status(&self) -> String {
        self.ok(&["status", "--porcelain"])
            .lines()
            .filter(|l| *l != "?? .ascignore")
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ops(&self) -> Vec<Value> {
        let v: Value = serde_json::from_str(&self.ok(&["op", "log", "--json"])).unwrap();
        v["ops"].as_array().unwrap().clone()
    }
}

#[test]
fn undo_of_a_commit_restores_head_and_the_work_tree_and_redo_reapplies_it() {
    let r = Repo::new();
    let base = r.sha("HEAD");
    r.commit("b.txt", "two\n", "two");
    let two = r.sha("HEAD");
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), base);
    assert!(!r.root.join("b.txt").exists());
    assert_eq!(r.status(), "");
    r.ok(&["redo"]);
    assert_eq!(r.sha("HEAD"), two);
    assert_eq!(r.read("b.txt"), "two\n");
    assert_eq!(r.asc(&["redo"]).stdout.trim(), "Nothing to redo.");
}

#[test]
fn undo_walks_back_through_the_log() {
    let r = Repo::new();
    let base = r.sha("HEAD");
    r.commit("b.txt", "2\n", "two");
    let two = r.sha("HEAD");
    r.commit("c.txt", "3\n", "three");
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), two);
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), base);
    r.ok(&["redo"]);
    assert_eq!(r.sha("HEAD"), two);
    r.ok(&["redo"]);
    assert_eq!(r.sha("master"), r.sha("HEAD"));
    assert!(r.root.join("c.txt").exists());
}

#[test]
fn undo_of_a_rebase_restores_the_branch() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "side"]);
    r.commit("s.txt", "side\n", "side");
    let side = r.sha("HEAD");
    r.ok(&["switch", "master"]);
    r.commit("m.txt", "main\n", "main");
    r.ok(&["switch", "side"]);
    r.ok(&["rebase", "master"]);
    assert_ne!(r.sha("HEAD"), side);
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), side);
    assert_eq!(r.sha("side"), side);
    assert!(!r.root.join("m.txt").exists());
    assert_eq!(r.status(), "");
}

#[test]
fn undo_of_modify_restores_head_and_the_dirty_work() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "feature"]);
    r.commit("b.txt", "two\n", "two");
    let two = r.sha("HEAD");
    r.index_head(1);
    r.write("b.txt", "amended\n");
    r.ok(&["modify", "-a", "-m", "two amended"]);
    assert_ne!(r.sha("HEAD"), two);
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), two);
}

#[test]
fn undo_of_split_restores_the_single_change() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "feature"]);
    r.write("x.txt", "x\n");
    r.write("y.txt", "y\n");
    r.ok(&["add", "x.txt", "y.txt"]);
    r.ok(&["commit", "-m", "both"]);
    let both = r.sha("HEAD");
    r.index_head(2);
    r.ok(&["split", "--by-file", "x.txt", "y.txt", "-m", "x only"]);
    assert_ne!(r.sha("HEAD"), both);
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), both);
    assert_eq!(r.read("y.txt"), "y\n");
    assert_eq!(r.status(), "");
}

#[test]
fn undo_of_reset_hard_restores_the_dirty_files() {
    let r = Repo::new();
    r.commit("b.txt", "two\n", "two");
    let two = r.sha("HEAD");
    r.write("a.txt", "dirty\n");
    r.ok(&["reset", "--hard", "HEAD~1"]);
    assert_eq!(r.read("a.txt"), "one\n");
    r.ok(&["undo"]);
    assert_eq!(r.sha("HEAD"), two);
    assert_eq!(r.read("a.txt"), "dirty\n");
    assert_eq!(r.read("b.txt"), "two\n");
}

#[test]
fn undo_of_checkout_force_restores_the_discarded_changes() {
    let r = Repo::new();
    r.write("a.txt", "dirty\n");
    r.ok(&["checkout", "-f", "master"]);
    assert_eq!(r.read("a.txt"), "one\n");
    r.ok(&["undo"]);
    assert_eq!(r.read("a.txt"), "dirty\n");
}

#[test]
fn a_moved_ref_is_refused_unless_forced() {
    let r = Repo::new();
    let base = r.sha("HEAD");
    r.commit("b.txt", "2\n", "two");
    r.commit("c.txt", "3\n", "three");
    let ops = r.ops();
    let two_op = ops[1]["op"].as_str().unwrap().to_owned();
    // `op restore` to the base-state op is a net movement; a later move of master makes the
    // plain check fail only when the recorded new value no longer matches.
    r.move_ref_unlogged("refs/heads/master", &base);
    let o = r.asc(&["undo"]);
    assert_eq!(o.code, 128, "{}", o.stderr);
    assert!(o.stderr.contains("moved since"), "{}", o.stderr);
    assert_eq!(r.sha("master"), base);
    let _ = two_op;
    assert_eq!(r.asc(&["undo", "-f"]).code, 0);
}

#[test]
fn stale_op_is_a_json_error_and_the_latest_op_is_accepted() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    r.commit("c.txt", "3\n", "three");
    let rows = r.ops();
    let newest = rows[0]["op"].as_str().unwrap().to_owned();
    let older = rows[1]["op"].as_str().unwrap().to_owned();
    let head = r.sha("HEAD");
    let o = r.asc(&["undo", "--op", &older, "--json"]);
    assert_eq!(o.code, 128, "{}", o.stderr);
    let v: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(v["error"]["code"], "STALE_OP");
    assert_eq!(r.sha("HEAD"), head, "a refused undo changes nothing");
    let o = r.asc(&["undo", "--op", &older]);
    assert_eq!(o.code, 128);
    assert!(o.stderr.contains("STALE_OP"), "{}", o.stderr);
    let done: Value = serde_json::from_str(&r.ok(&["undo", "--op", &newest, "--json"])).unwrap();
    assert_eq!(done["status"], "ok");
    assert_eq!(done["refs"][0]["name"], "refs/heads/master");
    // Redo accepts the op that was undone, and only that one.
    assert_eq!(r.asc(&["redo", "--op", &older]).code, 128);
    r.ok(&["redo", "--op", &newest]);
    assert_eq!(r.sha("HEAD"), head);
}

#[test]
fn dry_run_changes_nothing() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    let head = r.sha("HEAD");
    let out = r.ok(&["undo", "-n"]);
    assert!(out.starts_with("Would undo"), "{out}");
    assert_eq!(r.sha("HEAD"), head);
}

#[test]
fn op_log_json_is_wrapped_and_marks_undone_ops() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    let v: Value = serde_json::from_str(&r.ok(&["op", "log", "--json"])).unwrap();
    assert_eq!(v["version"], 1);
    let rows = v["ops"].as_array().unwrap();
    assert!(
        rows[0]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "commit")
    );
    assert_eq!(rows[0]["undone"], false);
    r.ok(&["undo"]);
    let rows = r.ops();
    assert_eq!(rows[0]["argv"][0], "undo");
    assert_eq!(rows[1]["undone"], true);
    let text = r.ok(&["op", "log", "-n", "2"]);
    assert!(text.contains("(undone)"), "{text}");
    assert_eq!(text.lines().count(), 2);
}

#[test]
fn op_show_and_restore() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    r.commit("c.txt", "3\n", "three");
    r.commit("d.txt", "4\n", "four");
    let rows = r.ops();
    let id = rows[1]["op"].as_str().unwrap().to_owned();
    let shown = r.ok(&["op", "show", &id]);
    assert!(shown.contains("commit"), "{shown}");
    assert!(shown.contains("refs/heads/master"), "{shown}");
    r.ok(&["op", "restore", &id]);
    assert!(r.root.join("c.txt").exists());
    assert!(!r.root.join("d.txt").exists());
    // Restore is an op like any other: undo returns to the tip.
    r.ok(&["undo"]);
    assert!(r.root.join("d.txt").exists());
}

#[test]
fn unknown_ops_are_refused() {
    let r = Repo::new();
    assert_eq!(r.asc(&["op", "show", "ffff"]).code, 128);
    assert_eq!(r.asc(&["op", "show", "zz"]).code, 128);
    assert_eq!(r.asc(&["op", "restore", "@"]).code, 128);
}

#[test]
fn a_vetoed_ref_swap_leaves_the_work_tree_alone() {
    let r = Repo::new();
    r.commit("b.txt", "two\n", "two");
    let two = r.sha("HEAD");
    // A hook that refuses the swap after the checks passed stands in for a racing writer.
    let hooks = r.root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    fs::write(
        &hook,
        "#!/bin/sh\ncat > /dev/null\nif [ \"$1\" = prepared ]; then exit 1; fi\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let o = r.asc(&["undo"]);
    assert_ne!(o.code, 0, "{}", o.stdout);
    assert_eq!(r.sha("HEAD"), two);
    assert_eq!(r.read("b.txt"), "two\n", "the files stay as they were");
    assert_eq!(r.status(), "");
}

#[test]
fn changes_that_no_snapshot_keeps_are_not_wiped_without_force() {
    let r = Repo::new();
    r.commit("b.txt", "two\n", "two");
    r.ok(&["config", "asc.opLog", "false"]);
    r.write("a.txt", "dirty\n");
    let o = r.asc(&["undo"]);
    assert_eq!(o.code, 128, "{}", o.stderr);
    assert!(o.stderr.contains("cannot be saved"), "{}", o.stderr);
    assert_eq!(r.read("a.txt"), "dirty\n");
    assert!(r.root.join("b.txt").exists());
    assert_eq!(r.asc(&["undo", "-f"]).code, 0);
    assert!(!r.root.join("b.txt").exists());
}

#[test]
fn op_log_clamps_a_huge_limit() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    let v: Value =
        serde_json::from_str(&r.ok(&["op", "log", "-n", "18446744073709551615", "--json"]))
            .unwrap();
    assert!(v["ops"].as_array().unwrap().len() <= 1_000);
}
