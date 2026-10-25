//! C27 acceptance: `asc evolog` and `asc interdiff`, driven through the built binary. Two
//! commits are indexed as versions 1 and 2 of one change directly in `state.db`.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

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

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    let out = sb
        .asc(cwd)
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

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let o = asc(sb, cwd, args);
    assert_eq!(o.code, 0, "asc {args:?}: {}", o.stderr);
    o.stdout
}

fn change_id(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

impl Repo {
    /// `master` has one commit; `feature` adds two (`one`, `two`), checked out at `two`.
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        ok(&sb, &root, &["init"]);
        ok(&sb, &root, &["commit", "--allow-empty", "-m", "base"]);
        ok(&sb, &root, &["switch", "-c", "feature"]);
        fs::write(
            root.join("a.txt"),
            "1
",
        )
        .unwrap();
        fs::write(
            root.join("b.txt"),
            "same
",
        )
        .unwrap();
        ok(&sb, &root, &["add", "."]);
        ok(&sb, &root, &["commit", "-m", "one"]);
        fs::write(
            root.join("a.txt"),
            "2
",
        )
        .unwrap();
        ok(&sb, &root, &["add", "."]);
        ok(&sb, &root, &["commit", "-m", "two"]);
        Self { sb, root }
    }

    fn asc(&self, args: &[&str]) -> Out {
        asc(&self.sb, &self.root, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        ok(&self.sb, &self.root, args)
    }

    fn sha(&self, rev: &str) -> String {
        self.ok(&["rev-parse", rev]).trim().to_owned()
    }

    /// Indexes `rev` as a version of change 1, with `pred` as its predecessor.
    fn index(&self, rev: &str, op: RewriteOp, pred: Option<&str>) {
        let sha = self.sha(rev);
        let pred = pred.map(|p| self.sha(p));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let id = |s: &str| {
                let oid: GitOid = s.parse().unwrap();
                oid
            };
            let cs = ChangesetId::from_bytes(
                git_map::git_to_asc(&db, ws.ws.repo, id(&sha), ObjectKind::Commit)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            let mut preds = Vec::new();
            if let Some(p) = &pred {
                preds.push(ChangesetId::from_bytes(
                    git_map::git_to_asc(&db, ws.ws.repo, id(p), ObjectKind::Commit)
                        .await
                        .unwrap()
                        .unwrap(),
                ));
            }
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
                change: change_id(1),
                meta: MetaId::from_bytes([1; 32]),
                op,
            };
            rows.insert(&row, &preds).await.unwrap();
        });
    }

    fn versions(&self) {
        self.index("HEAD~1", RewriteOp::New, None);
        self.index("HEAD", RewriteOp::Amend, Some("HEAD~1"));
    }
}

#[test]
fn evolog_json_lists_versions_oldest_first() {
    let r = Repo::new();
    r.versions();
    let v: Value = serde_json::from_str(r.ok(&["evolog", "--json"]).trim()).expect("json");
    assert_eq!(v["version"], 1);
    let vs = v["versions"].as_array().unwrap();
    assert_eq!(vs.len(), 2);
    assert_eq!(vs[0]["version"], 1);
    assert_eq!(vs[0]["commit"], r.sha("HEAD~1"));
    assert_eq!(vs[0]["op"], "new");
    assert_eq!(vs[1]["version"], 2);
    assert_eq!(vs[1]["commit"], r.sha("HEAD"));
    assert_eq!(vs[1]["op"], "amend");
    assert_eq!(vs[1]["preds"][0], r.sha("HEAD~1"));
    assert_eq!(vs[1]["published"], false);
}

#[test]
fn evolog_text_is_newest_first() {
    let r = Repo::new();
    r.versions();
    let out = r.ok(&["evolog"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("v2  "), "{out}");
    assert!(lines[1].starts_with("v1  "), "{out}");
}

#[test]
fn interdiff_defaults_to_the_last_two_versions() {
    let r = Repo::new();
    r.versions();
    let out = r.ok(&["interdiff", "HEAD"]);
    assert!(out.contains("diff --git a/a.txt b/a.txt"), "{out}");
    assert!(
        out.contains(
            "-1
+2"
        ),
        "{out}"
    );
    assert!(!out.contains("b.txt"), "{out}");
    let same = r.ok(&["interdiff", "HEAD", "2", "2"]);
    assert_eq!(same, "");
    let named = r.ok(&["interdiff", "HEAD", "1", "2"]);
    assert_eq!(named, out);
    let rev = r.ok(&["interdiff", "HEAD", "2", "1"]);
    assert!(
        rev.contains(
            "-2
+1"
        ),
        "{rev}"
    );
}

#[test]
fn interdiff_rejects_missing_versions_and_bad_usage() {
    let r = Repo::new();
    r.versions();
    let o = r.asc(&["interdiff", "HEAD", "1", "9"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("2 versions"), "{}", o.stderr);
    let o = r.asc(&["interdiff", "HEAD", "x", "2"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("bad version"), "{}", o.stderr);
    let o = r.asc(&["interdiff"]);
    assert_ne!(o.code, 0);
}

#[test]
fn interdiff_of_a_single_version_is_an_error() {
    let r = Repo::new();
    r.index("HEAD", RewriteOp::New, None);
    let o = r.asc(&["interdiff", "HEAD"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("only one version"), "{}", o.stderr);
}

#[test]
fn evolog_patch_prints_each_version() {
    let r = Repo::new();
    r.versions();
    let out = r.ok(&["evolog", "-p"]);
    assert!(out.contains("+2"), "{out}");
    assert!(out.contains("+1"), "{out}");
}

#[test]
fn evolog_falls_back_to_local_versions_when_the_server_is_unreachable() {
    let r = Repo::new();
    r.versions();
    // Nothing listens on port 1.
    r.ok(&["remote", "add", "origin", "asc+http://127.0.0.1:1/acme/app"]);
    let v: Value = serde_json::from_str(r.ok(&["evolog", "--json"]).trim()).expect("json");
    let vs = v["versions"].as_array().unwrap();
    assert_eq!(vs.len(), 2);
    assert_eq!(vs[0]["published"], false);
    assert_eq!(vs[1]["published"], false);
    assert_eq!(r.asc(&["evolog"]).stderr, "");
}
