//! C33 acceptance: `asc stack exec`, driven through the built binary. The change index is filled
//! directly in `state.db`, as in the C09 tests.


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
    /// `master` has `base`; a stack `one` < `two` < `three` sits above it, checked out at
    /// `three`. Changes are numbered 1, 2, 3.
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(sb.user.join(".gitconfig"), "[user]\n\tname = T\n\temail = t@example.com\n").unwrap();
        let root = sb.dir("repo");
        let r = Self { sb, root };
        r.ok(&["init"]);
        r.ok(&["commit", "--allow-empty", "-m", "base"]);
        r.ok(&["switch", "-c", "feature"]);
        for m in ["one", "two", "three"] {
            fs::write(r.root.join(m), m).unwrap();
            r.ok(&["add", m]);
            r.ok(&["commit", "-m", m]);
        }
        r.index("HEAD~2", 1);
        r.index("HEAD~1", 2);
        r.index("HEAD", 3);
        r
    }

    fn asc(&self, args: &[&str]) -> Out {
        let out = self.sb.asc(&self.root).args(args).stdin(std::process::Stdio::null()).output().expect("spawn");
        Out { code: out.status.code().unwrap_or(-1), stdout: text(&out.stdout), stderr: text(&out.stderr) }
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.asc(args);
        assert_eq!(o.code, 0, "asc {args:?}: {}", o.stderr);
        o.stdout
    }

    fn sha(&self, rev: &str) -> String {
        self.ok(&["rev-parse", rev]).trim().to_owned()
    }

    fn index(&self, rev: &str, n: u8) {
        let sha = self.sha(rev);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let oid: GitOid = sha.parse().unwrap();
            let cs = git_map::git_to_asc(&db, ws.ws.repo, oid, ObjectKind::Commit).await.unwrap().unwrap();
            let rows = StateChanges::new(db.clone(), ws.ws.repo);
            let row = ChangeRow {
                cs: ChangesetId::from_bytes(cs),
                change: change_id(n),
                meta: MetaId::from_bytes([n; 32]),
                op: RewriteOp::New,
            };
            rows.insert(&row, &[]).await.unwrap();
        });
    }
}

fn subjects(v: &Value) -> Vec<String> {
    v["results"].as_array().unwrap().iter().map(|r| r["stdout"].as_str().unwrap().trim().to_owned()).collect()
}

fn cached(v: &Value) -> Vec<bool> {
    v["results"].as_array().unwrap().iter().map(|r| r["cached"].as_bool().unwrap()).collect()
}

#[test]
fn runs_on_each_change_bottom_to_top_then_hits_the_cache() {
    let r = Repo::new();
    let head = r.sha("HEAD");
    let o = r.asc(&["stack", "exec", "--json", "--", "git", "log", "-1", "--format=%s"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["version"], 1);
    assert_eq!(v["ok"], true);
    assert_eq!(subjects(&v), ["one", "two", "three"]);
    assert_eq!(cached(&v), [false, false, false]);
    // The user's own work tree is untouched.
    assert_eq!(r.sha("HEAD"), head);

    let again: Value = serde_json::from_str(&r.ok(&["stack", "exec", "--json", "--", "git", "log", "-1", "--format=%s"])).unwrap();
    assert_eq!(subjects(&again), ["one", "two", "three"]);
    assert_eq!(cached(&again), [true, true, true]);

    // Another command line has its own cache.
    let other: Value = serde_json::from_str(&r.ok(&["stack", "exec", "--json", "--", "git", "log", "-1", "--format=%an"])).unwrap();
    assert_eq!(cached(&other), [false, false, false]);
}

#[test]
fn a_failing_command_exits_1_and_is_reported() {
    let r = Repo::new();
    let o = r.asc(&["stack", "exec", "--json", "--", "git", "rev-parse", "--verify", "nosuchref"]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_ne!(v["results"][0]["exit"], 0);
    let h = r.asc(&["stack", "exec", "--", "git", "rev-parse", "--verify", "nosuchref"]);
    assert_eq!(h.code, 1);
    assert!(h.stdout.starts_with("fail("), "{}", h.stdout);
}

#[test]
fn revset_narrows_the_changes() {
    let r = Repo::new();
    let v: Value =
        serde_json::from_str(&r.ok(&["stack", "exec", "--json", "--revset", "git(feature)", "--", "git", "log", "-1", "--format=%s"]))
            .unwrap();
    assert_eq!(subjects(&v), ["three"]);
}

#[test]
fn a_command_is_required_and_a_bad_revset_is_usage() {
    let r = Repo::new();
    assert_eq!(r.asc(&["stack", "exec"]).code, 129);
    assert_eq!(r.asc(&["stack", "exec", "--revset", "((", "--", "git", "status"]).code, 129);
}
