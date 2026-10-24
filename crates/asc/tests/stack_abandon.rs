//! C51 acceptance: `asc uncommit`, `asc unamend` and `asc abandon`, driven through the built
//! binary. The change index is filled directly in `state.db`: `feature` holds the stack
//! one < two < three (changes 1, 2, 3); `two` touches two files.

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
        r.commit(&[("three", "three"), ("g", "g")], "three");
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

    fn files(&self, rev: &str) -> Vec<String> {
        self.ok(&["ls-tree", "--name-only", rev])
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[test]
fn uncommit_moves_the_tree_back_to_the_stage_and_keeps_the_change_id() {
    let r = Repo::new();
    let before_tree = r.tree("feature");
    let out = r.asc(&["uncommit"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Uncommitted "), "{}", out.stdout);
    assert_eq!(r.subjects("feature"), ["three", "two", "one", "base"]);
    assert_eq!(r.tree("feature"), r.tree("feature~1"));
    assert_eq!(r.status(), "A  g\nA  three\n");
    assert_eq!(fs::read_to_string(r.root.join("three")).unwrap(), "three");
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(r.change("feature"), change_id(3).to_string());
    // Committing the stage again gives back the tree.
    r.ok(&["commit", "-m", "again"]);
    assert_eq!(r.tree("feature"), before_tree);
}

#[test]
fn uncommit_by_path_moves_only_those_paths() {
    let r = Repo::new();
    let out = r.asc(&["uncommit", "g"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.status(), "A  g\n");
    assert!(r.files("feature").contains(&"three".to_owned()));
    assert!(!r.files("feature").contains(&"g".to_owned()));
    assert_eq!(r.change("feature"), change_id(3).to_string());
}

#[test]
fn uncommit_refuses_what_it_cannot_do() {
    let r = Repo::new();
    let tip = r.sha("feature");
    // A path the change does not touch, a prompt, a change that is not checked out, a change
    // with changes above it.
    assert_eq!(r.asc(&["uncommit", "nosuch"]).code, 1);
    assert_eq!(r.asc(&["uncommit", "-p"]).code, 1);
    assert_eq!(r.asc(&["uncommit", "feature~1"]).code, 128);
    assert_eq!(r.sha("feature"), tip);
    r.ok(&["switch", "--detach", "feature~1"]);
    assert_eq!(r.asc(&["uncommit"]).code, 1);
    assert_eq!(r.sha("feature"), tip);
}

#[test]
fn uncommit_dry_run_changes_nothing() {
    let r = Repo::new();
    let tip = r.sha("feature");
    let v: Value = serde_json::from_str(&r.ok(&["uncommit", "-n", "--json"])).unwrap();
    assert_eq!(v["verb"].as_str(), Some("uncommit"));
    assert_eq!(r.sha("feature"), tip);
    assert_eq!(r.status(), "");
}

#[test]
fn unamend_restores_the_previous_version_and_stages_the_amend() {
    let r = Repo::new();
    let before = r.tree("feature");
    r.write("extra", "x");
    r.ok(&["add", "extra"]);
    r.ok(&["modify", "-m", "three v2"]);
    assert_eq!(r.subjects("feature")[0], "three v2");
    assert_eq!(r.change("feature"), change_id(3).to_string());
    let out = r.asc(&["unamend"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Unamended "), "{}", out.stdout);
    assert_eq!(r.subjects("feature"), ["three", "two", "one", "base"]);
    assert_eq!(r.tree("feature"), before);
    assert_eq!(r.status(), "A  extra\n");
    assert_eq!(r.change("feature"), change_id(3).to_string());
    // A change that was never amended has nothing to restore.
    r.ok(&["reset", "--hard", "HEAD"]);
    r.ok(&["switch", "--detach", "feature~1"]);
    assert_eq!(r.asc(&["unamend"]).code, 1);
}

#[test]
fn abandon_the_top_change_moves_head_and_the_branch_to_the_parent() {
    let r = Repo::new();
    let two = r.sha("feature~1");
    let out = r.asc(&["abandon"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Abandoned "), "{}", out.stdout);
    assert_eq!(r.sha("feature"), two);
    assert_eq!(r.sha("HEAD"), two);
    assert_eq!(r.status(), "");
    assert!(!r.root.join("three").exists());
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
}

#[test]
fn abandon_a_middle_change_restacks_the_children_onto_its_parent() {
    let r = Repo::new();
    let v: Value = serde_json::from_str(&r.ok(&["abandon", "--json", "feature~1"])).unwrap();
    assert_eq!(
        (v["status"].as_str(), v["verb"].as_str()),
        (Some("complete"), Some("abandon"))
    );
    assert_eq!(v["restacked"].as_u64(), Some(1));
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.status(), "");
    assert!(r.root.join("three").exists() && r.root.join("one").exists());
    assert!(!r.root.join("t1").exists());
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
}

#[test]
fn abandon_several_changes_in_one_run() {
    let r = Repo::new();
    let out = r.asc(&["abandon", "feature~2", "feature~1", "--keep-descendants"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stdout
            .lines()
            .filter(|l| l.starts_with("Abandoned "))
            .count(),
        2,
        "{}",
        out.stdout
    );
    assert_eq!(r.subjects("feature"), ["three", "base"]);
    assert_eq!(r.status(), "");
}

#[test]
fn abandon_refuses_trunk_and_dirty_work_and_dry_run_changes_nothing() {
    let r = Repo::new();
    let tip = r.sha("feature");
    // A change on trunk is not a draft.
    assert_eq!(r.asc(&["abandon", "master"]).code, 128);
    let v: Value = serde_json::from_str(&r.ok(&["abandon", "-n", "--json", "feature~1"])).unwrap();
    assert_eq!(v["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(r.sha("feature"), tip);
    // Uncommitted work blocks abandoning the checked-out change.
    r.write("one", "edited");
    assert_eq!(r.asc(&["abandon"]).code, 1);
    assert_eq!(r.sha("feature"), tip);
    assert_eq!(fs::read_to_string(r.root.join("one")).unwrap(), "edited");
}
