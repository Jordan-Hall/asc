//! C13 acceptance: `asc split`, `asc squash` and `asc fold`, driven through the built binary.
//! The change index is filled directly in `state.db`, as in the C10 tests: `feature` holds the
//! stack one < two < three (changes 1, 2, 3); `two` touches two files so it can be split.

mod support;

use std::fs;
use std::path::PathBuf;

use asc_core::change::{ChangeId, MetaId, RewriteOp};
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid};
use asc_engine::change::{ChangeRow, ChangeRows};
use asc_state::changes::StateChanges;
use asc_state::state::{git_map, workspaces};
use asc_state::{AscHome, StateDb};
use serde_json::Value;
use support::{Sandbox, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn change_id(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
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
        r.write("f", "a\nb\nc\n");
        r.ok(&["add", "f"]);
        r.ok(&["commit", "-m", "base"]);
        r.ok(&["switch", "-c", "feature"]);
        r.commit(&[("one", "one")], "one");
        r.commit(&[("t1", "t1"), ("t2", "t2")], "two");
        r.commit(&[("three", "three")], "three");
        r.index("HEAD~2", 1);
        r.index("HEAD~1", 2);
        r.index("HEAD", 3);
        r
    }

    fn commit(&self, files: &[(&str, &str)], msg: &str) {
        for (n, b) in files {
            self.write(n, b);
            self.ok(&["add", n]);
        }
        self.ok(&["commit", "-m", msg]);
    }

    fn write(&self, name: &str, body: &str) {
        fs::write(self.root.join(name), body).unwrap();
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

    fn tree(&self, rev: &str) -> String {
        self.sha(&format!("{rev}^{{tree}}"))
    }

    fn subjects(&self, rev: &str) -> Vec<String> {
        self.ok(&["log", "--format=%s", rev])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn status(&self) -> String {
        self.ok(&["status", "--porcelain"])
            .lines()
            .filter(|l| *l != "?? .ascignore")
            .map(|l| format!("{l}\n"))
            .collect()
    }

    fn index(&self, rev: &str, n: u8) {
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
            // `asc commit` already indexed the commit under a fresh id; the index keeps the
            // first row, so drop it to give the commit the id this scenario names.
            for sql in [
                "DELETE FROM change_preds WHERE repo_id = ? AND cs = ?",
                "DELETE FROM changes WHERE repo_id = ? AND cs = ?",
            ] {
                sqlx::query(sql)
                    .bind(ws.ws.repo.0)
                    .bind(cs.as_bytes().to_vec())
                    .execute(db.pool())
                    .await
                    .unwrap();
            }
            let rows = StateChanges::new(db.clone(), ws.ws.repo);
            let row = ChangeRow {
                cs,
                change: change_id(n),
                meta: MetaId::from_bytes([n; 32]),
                op: RewriteOp::New,
            };
            rows.insert(&row, &[]).await.unwrap();
        });
    }

    fn change(&self, rev: &str) -> String {
        self.ok(&["change-id", rev]).trim().to_owned()
    }
}

#[test]
fn split_by_file_keeps_the_id_on_the_top_piece_and_restacks_the_children() {
    let r = Repo::new();
    let old_head_tree = r.tree("feature");
    let two_tree = r.tree("feature~1");
    let out = r.asc(&[
        "split",
        "--change",
        "feature~1",
        "--by-file",
        "t1",
        "-m",
        "only t1",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 2, "{}", out.stdout);
    assert_eq!(
        r.subjects("feature"),
        ["three", "two", "only t1", "one", "base"]
    );
    // The top piece has the original tree, so the children and the work tree are unchanged.
    assert_eq!(r.tree("feature~1"), two_tree);
    assert_eq!(r.tree("feature"), old_head_tree);
    assert_eq!(r.status(), "");
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    // The piece that keeps the message keeps the id; the other piece has a new one.
    assert_eq!(r.change("feature~1"), change_id(2).to_string());
    let lower = r.change("feature~2");
    assert!(
        !lower.is_empty() && lower != change_id(2).to_string() && lower != change_id(1).to_string()
    );
    assert_eq!(r.change("feature~3"), change_id(1).to_string());
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(
        r.ok(&["ls-tree", "--name-only", "feature~2"])
            .lines()
            .filter(|l| l.starts_with('t'))
            .count(),
        1
    );
}

#[test]
fn split_refuses_what_it_cannot_do_without_a_prompt() {
    let r = Repo::new();
    let head = r.sha("feature");
    assert_eq!(r.asc(&["split"]).code, 129);
    assert_eq!(r.asc(&["split", "--by-hunk"]).code, 1);
    assert_eq!(r.asc(&["split", "--by-commit"]).code, 1);
    // A pathspec the change does not touch, and one that covers everything.
    assert_eq!(r.asc(&["split", "--by-file", "nosuch"]).code, 1);
    assert_eq!(
        r.asc(&["split", "--change", "feature~1", "--by-file", "."])
            .code,
        1
    );
    assert_eq!(r.sha("feature"), head);
}

#[test]
fn fold_merges_a_change_into_its_parent_and_reparents_its_children() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    let out = r.asc(&["fold", "feature~1"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Folded "), "{}", out.stdout);
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.tree("feature"), tip_tree);
    assert_eq!(r.status(), "");
    // The earliest folded change's id survives, the child keeps its own.
    assert_eq!(r.change("feature~1"), change_id(1).to_string());
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
}

#[test]
fn fold_keep_takes_the_folded_changes_message() {
    let r = Repo::new();
    assert_eq!(r.asc(&["fold", "--keep", "feature~1"]).code, 0);
    assert_eq!(r.subjects("feature"), ["three", "two", "base"]);
    assert_eq!(r.change("feature~1"), change_id(1).to_string());
}

#[test]
fn fold_stack_takes_everything_above_and_the_bottom_change_has_no_parent_to_fold_into() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    assert_eq!(r.asc(&["fold", "feature~2"]).code, 1);
    let v: Value =
        serde_json::from_str(&r.ok(&["fold", "--stack", "--json", "feature~1"])).unwrap();
    assert_eq!(
        (v["status"].as_str(), v["verb"].as_str()),
        (Some("complete"), Some("fold"))
    );
    assert_eq!(r.subjects("feature"), ["one", "base"]);
    assert_eq!(r.tree("feature"), tip_tree);
    assert_eq!(r.change("feature"), change_id(1).to_string());
}

#[test]
fn squash_collapses_the_stack_into_the_bottom_change() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    let out = r.asc(&["squash"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Squashed "), "{}", out.stdout);
    assert_eq!(r.subjects("feature"), ["one", "base"]);
    assert_eq!(r.tree("feature"), tip_tree);
    assert_eq!(r.change("feature"), change_id(1).to_string());
    assert_eq!(r.status(), "");
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    // One change left: nothing to do.
    let again: Value = serde_json::from_str(&r.ok(&["squash", "--json"])).unwrap();
    assert_eq!(again["status"], "nothing");
}

#[test]
fn squash_takes_a_message_and_refuses_the_editor() {
    let r = Repo::new();
    assert_eq!(r.asc(&["squash", "-e"]).code, 1);
    assert_eq!(r.asc(&["squash", "-m", "all of it"]).code, 0);
    assert_eq!(r.subjects("feature"), ["all of it", "base"]);
}

#[test]
fn squash_from_the_middle_restacks_the_changes_above() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    r.ok(&["switch", "--detach", "feature~1"]);
    let out = r.asc(&["squash"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.tree("feature"), tip_tree);
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.change("feature~1"), change_id(1).to_string());
    // HEAD followed the squashed change.
    assert_eq!(r.sha("HEAD"), r.sha("feature~1"));
    assert_eq!(r.status(), "");
}
