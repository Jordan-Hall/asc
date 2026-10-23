//! C14 acceptance: `asc move`, `asc reorder` and `asc stack rename|delete|pop`, driven through the
//! built binary. The change index is filled directly in `state.db`, as in the C13 tests:
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

    /// An editor command (run through `sh`) that rewrites the todo file. `body` is a shell
    /// snippet over `$1` (the file) and `$t` (its non-comment lines).
    fn editor(&self, name: &str, body: &str) -> String {
        let p = self.sb.dir("editors").join(name);
        fs::write(
            &p,
            format!("t=$(grep -v '^#' \"$1\" | sed '/^$/d')\n{body}\n"),
        )
        .unwrap();
        format!("sh '{}'", p.to_string_lossy().replace('\\', "/"))
    }
}

/// Lines `a b c` (1-based positions of the todo list) written back in that order.
fn order(a: u8, b: u8, c: u8) -> String {
    format!(
        "{{ echo \"$t\" | sed -n {a}p; echo \"$t\" | sed -n {b}p; echo \"$t\" | sed -n {c}p; }} > \"$1\""
    )
}

#[test]
fn move_reparents_a_change_and_the_changes_above_it() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    let out = r.asc(&["move", "--source", "feature~2", "--onto", "side"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Moved "), "{}", out.stdout);
    assert_eq!(
        r.subjects("feature"),
        ["three", "two", "one", "side", "base"]
    );
    assert_ne!(r.tree("feature"), tip_tree);
    assert_eq!(r.sha("feature~3"), r.sha("side"));
    assert_eq!(r.change("feature~2"), change_id(1).to_string());
    assert_eq!(r.change("feature~1"), change_id(2).to_string());
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.status(), "");
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    // Already there: nothing to do.
    let v: Value =
        serde_json::from_str(&r.ok(&["move", "--onto", "side", "--source", "feature~2", "--json"]))
            .unwrap();
    assert_eq!(v["status"], "nothing");
}

#[test]
fn move_dry_run_moves_nothing_and_reports() {
    let r = Repo::new();
    let before = r.sha("feature");
    let v: Value = serde_json::from_str(&r.ok(&[
        "move",
        "-n",
        "--json",
        "--source",
        "feature~2",
        "--onto",
        "side",
    ]))
    .unwrap();
    assert_eq!(
        (v["status"].as_str(), v["verb"].as_str()),
        (Some("complete"), Some("move"))
    );
    assert_eq!(v["changes"].as_array().unwrap().len(), 3);
    assert_eq!(r.sha("feature"), before);
    assert_eq!(r.status(), "");
}

#[test]
fn move_only_leaves_the_children_on_the_old_parent() {
    let r = Repo::new();
    let out = r.asc(&["move", "--only", "--source", "feature~1", "--onto", "side"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    // `two` now sits on `side`, off the branch; `three` went onto `one`.
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.change("feature~1"), change_id(1).to_string());
    assert_eq!(r.status(), "");
}

#[test]
fn move_refuses_a_cycle_a_move_onto_itself_and_a_missing_target() {
    let r = Repo::new();
    let before = r.sha("feature");
    assert_eq!(
        r.asc(&["move", "--source", "feature~2", "--onto", "feature"])
            .code,
        1
    );
    assert_eq!(
        r.asc(&["move", "--source", "feature~1", "--onto", "feature~1"])
            .code,
        1
    );
    assert_eq!(r.asc(&["move", "--onto", "nosuchrev"]).code, 128);
    assert_eq!(r.asc(&["move"]).code, 129);
    assert_eq!(r.sha("feature"), before);
}

#[test]
fn reorder_rebuilds_the_stack_in_the_saved_order_and_keeps_the_ids() {
    let r = Repo::new();
    let tip_tree = r.tree("feature");
    // Branches follow their changes: `top` marks where `two` ends up.
    r.ok(&["branch", "top", "feature~1"]);
    let ed = r.editor("rot", &order(3, 1, 2));
    let out = r.run(&["reorder"], Some(&ed));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Reordered "), "{}", out.stdout);
    assert_eq!(r.subjects("top"), ["two", "one", "three", "base"]);
    assert_eq!(r.subjects("feature"), ["three", "base"]);
    assert_eq!(r.tree("top"), tip_tree);
    assert_eq!(r.change("top~2"), change_id(3).to_string());
    assert_eq!(r.change("top~1"), change_id(1).to_string());
    assert_eq!(r.change("top"), change_id(2).to_string());
    assert_eq!(r.status(), "");
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
}

#[test]
fn reorder_stops_on_a_bad_list_and_does_nothing_for_the_same_order() {
    let r = Repo::new();
    let before = r.sha("feature");
    let drop_one = r.editor("drop", "echo \"$t\" | sed -n 1,2p > \"$1\"");
    assert_eq!(r.run(&["reorder"], Some(&drop_one)).code, 1);
    let twice = r.editor("twice", &order(1, 1, 3));
    assert_eq!(r.run(&["reorder"], Some(&twice)).code, 1);
    let failing = r.editor("fail", "exit 1");
    assert_eq!(r.run(&["reorder"], Some(&failing)).code, 128);
    assert_eq!(r.sha("feature"), before);
    let v: Value = serde_json::from_str(&r.run(&["reorder", "--json"], Some(":")).stdout).unwrap();
    assert_eq!(v["status"], "nothing");
    assert_eq!(r.sha("feature"), before);
}

#[test]
fn reorder_dry_run_leaves_every_ref_alone() {
    let r = Repo::new();
    let before = r.sha("feature");
    let ed = r.editor("swap", &order(2, 1, 3));
    let out = r.run(&["reorder", "-n"], Some(&ed));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Would reorder "), "{}", out.stdout);
    assert_eq!(r.sha("feature"), before);
}

#[test]
fn stack_rename_renames_the_branch_head_is_on() {
    let r = Repo::new();
    assert_eq!(r.asc(&["stack", "rename", "-bad"]).code, 129);
    assert_eq!(r.asc(&["stack", "rename", "a b"]).code, 128);
    let out = r.asc(&["stack", "rename", "renamed"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "renamed");
    assert!(r.ok(&["branch", "--list", "feature"]).trim().is_empty());
    r.ok(&["switch", "--detach", "HEAD"]);
    assert_eq!(r.asc(&["stack", "rename", "other"]).code, 1);
}

#[test]
fn stack_pop_keeps_the_content_and_drops_the_branch() {
    let r = Repo::new();
    let two = r.sha("feature~1");
    let out = r.asc(&["stack", "pop"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Popped "), "{}", out.stdout);
    assert_eq!(r.sha("HEAD"), two);
    assert!(r.ok(&["branch", "--list", "feature"]).trim().is_empty());
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "");
    assert_eq!(r.status(), "A  three\n");
}

#[test]
fn stack_pop_in_the_middle_restacks_the_children() {
    let r = Repo::new();
    r.ok(&["switch", "--detach", "feature~1"]);
    let one = r.sha("feature~2");
    let out = r.asc(&["stack", "pop"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.sha("HEAD"), one);
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.status(), "A  t1\nA  t2\n");
}

#[test]
fn stack_delete_drops_the_change_and_restacks_the_rest() {
    let r = Repo::new();
    r.ok(&["switch", "--detach", "feature~1"]);
    let out = r.asc(&["stack", "delete", "-n"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Would delete "), "{}", out.stdout);
    assert_eq!(r.subjects("feature").len(), 4);
    assert_eq!(r.asc(&["stack", "delete", "-c"]).code, 1);
    let out = r.asc(&["stack", "delete"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.subjects("feature"), ["three", "one", "base"]);
    assert_eq!(r.sha("HEAD"), r.sha("feature~1"));
    assert_eq!(r.status(), "");
}

#[test]
fn stack_delete_takes_the_branch_head_is_on_and_optionally_the_changes_above() {
    let r = Repo::new();
    let two = r.sha("feature~1");
    let out = r.asc(&["stack", "delete"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(r.ok(&["branch", "--list", "feature"]).trim().is_empty());
    assert_eq!(r.sha("HEAD"), two);
    assert_eq!(r.status(), "");

    let r = Repo::new();
    let one = r.sha("feature~2");
    r.ok(&["switch", "--detach", "feature~1"]);
    let out = r.asc(&["stack", "delete", "--upstack"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(r.ok(&["branch", "--list", "feature"]).trim().is_empty());
    assert_eq!(r.sha("HEAD"), one);
    assert_eq!(r.status(), "");
}
