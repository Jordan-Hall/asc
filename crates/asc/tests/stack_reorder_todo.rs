//! P03 acceptance: `asc reorder --todo <file>`, driven through the built binary. The change index is filled directly in `state.db`, as in the C13 tests:
//! `feature` holds the stack one < two < three (changes 1, 2, 3) on `master`; `side` is a
//! commit next to it on `master`.

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
        r.ok(&["switch", "-c", "side"]);
        r.commit(&[("s", "side")], "side");
        r.ok(&["switch", "master"]);
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

    fn run(&self, args: &[&str], editor: Option<&str>) -> Out {
        let mut cmd = self.sb.asc(&self.root);
        cmd.args(args).stdin(std::process::Stdio::null());
        if let Some(e) = editor {
            cmd.env("GIT_SEQUENCE_EDITOR", e);
        }
        let out = cmd.output().expect("spawn");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: text(&out.stdout),
            stderr: text(&out.stderr),
        }
    }

    fn asc(&self, args: &[&str]) -> Out {
        self.run(args, None)
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

    /// Writes a todo that names the changes of `feature` (1 = one, 2 = two, 3 = three) as
    /// `(action, n, message)` items and returns its path.
    fn todo(&self, items: &[(&str, u8, Option<&str>)]) -> String {
        let list: Vec<Value> = items
            .iter()
            .map(|(a, n, m)| {
                let rev = ["", "feature~2", "feature~1", "feature"][*n as usize];
                let mut v = serde_json::json!({ "action": a, "change": self.change(rev) });
                if let Some(m) = m {
                    v["message"] = Value::from(*m);
                }
                v
            })
            .collect();
        let p = self.sb.dir("todos").join("todo.json");
        fs::write(&p, serde_json::json!({ "items": list }).to_string()).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn oplog_len(&self) -> usize {
        self.ok(&["op", "log"]).lines().count()
    }
}

#[test]
fn todo_reorders_and_rewords_in_one_operation() {
    let r = Repo::new();
    // Branches follow their changes: `top` marks where `two` ends up.
    r.ok(&["branch", "top", "feature~1"]);
    let ops = r.oplog_len();
    let f = r.todo(&[
        ("pick", 3, None),
        ("reword", 1, Some("uno")),
        ("pick", 2, None),
    ]);
    let out = r.asc(&["reorder", "--todo", &f]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.subjects("top"), ["two", "uno", "three", "base"]);
    assert_eq!(r.subjects("feature"), ["three", "base"]);
    assert_eq!(r.change("top~2"), change_id(3).to_string());
    assert_eq!(r.change("top~1"), change_id(1).to_string());
    assert_eq!(r.change("top"), change_id(2).to_string());
    assert_eq!(r.status(), "");
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(r.oplog_len(), ops + 1, "{}", r.ok(&["op", "log"]));
}

#[test]
fn todo_folds_into_the_change_below() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    let f = r.todo(&[("pick", 1, None), ("fold", 2, None), ("pick", 3, None)]);
    let out = r.asc(&["reorder", "--todo", &f]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.tree("feature"), tip_tree);
    assert_eq!(r.change("feature~1"), change_id(1).to_string());
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.status(), "");
}

#[test]
fn todo_drops_a_change_and_the_branch_follows() {
    let r = Repo::new();
    let f = r.todo(&[("pick", 1, None), ("pick", 2, None), ("drop", 3, None)]);
    let out = r.asc(&["reorder", "--todo", &f]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.subjects("feature"), ["two", "one", "base"]);
    assert_eq!(r.status(), "");
}

#[test]
fn todo_refuses_a_bad_list_and_dry_run_moves_nothing() {
    let r = Repo::new();
    let before = r.sha("feature");
    let missing = r.todo(&[("pick", 1, None), ("pick", 2, None)]);
    assert_eq!(r.asc(&["reorder", "--todo", &missing]).code, 1);
    let first_fold = r.todo(&[("fold", 1, None), ("pick", 2, None), ("pick", 3, None)]);
    assert_eq!(r.asc(&["reorder", "--todo", &first_fold]).code, 1);
    let no_msg = r.todo(&[("reword", 1, None), ("pick", 2, None), ("pick", 3, None)]);
    assert_eq!(r.asc(&["reorder", "--todo", &no_msg]).code, 1);
    let p = r.sb.dir("todos").join("junk.json");
    fs::write(&p, "{\"items\":[{\"action\":\"squash\",\"change\":\"x\"}]}").unwrap();
    assert_eq!(r.asc(&["reorder", "--todo", &p.to_string_lossy()]).code, 1);
    assert_eq!(r.asc(&["reorder", "--todo", "nonexistent.json"]).code, 1);
    assert_eq!(r.sha("feature"), before);
    let ok = r.todo(&[("pick", 3, None), ("pick", 2, None), ("pick", 1, None)]);
    let out = r.asc(&["reorder", "--todo", &ok, "-n"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Would reorder "), "{}", out.stdout);
    assert_eq!(r.sha("feature"), before);
    let same = r.todo(&[("pick", 1, None), ("pick", 2, None), ("pick", 3, None)]);
    let v: Value = serde_json::from_str(&r.ok(&["reorder", "--todo", &same, "--json"])).unwrap();
    assert_eq!(v["status"], "nothing");
}
