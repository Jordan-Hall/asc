//! C09 acceptance: `asc up|down|top|bottom|next|prev`, driven through the built binary. The
//! change index is filled directly in `state.db` (the sinks that mint ids on commit belong to
//! C04), so the stacks under test are exactly the rows written here.

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
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let r = Self { sb, root };
        r.ok(&["init"]);
        r.ok(&["commit", "--allow-empty", "-m", "base"]);
        r.ok(&["switch", "-c", "feature"]);
        for m in ["one", "two", "three"] {
            r.ok(&["commit", "--allow-empty", "-m", m]);
        }
        r.index("HEAD~2", 1);
        r.index("HEAD~1", 2);
        r.index("HEAD", 3);
        r
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

    fn head_subject(&self) -> String {
        self.ok(&["log", "-1", "--format=%s"]).trim().to_owned()
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
            let cs = git_map::git_to_asc(&db, ws.ws.repo, oid, ObjectKind::Commit)
                .await
                .unwrap()
                .unwrap();
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

    /// Adds a second child `alt` (change 4) of `one`, then checks `two`... out again at `back`.
    fn fork(&self, back: &str) {
        self.ok(&["switch", "--detach", &self.sha("HEAD~2")]);
        self.ok(&["branch", "-f", "alt", "HEAD"]);
        self.ok(&["switch", "alt"]);
        self.ok(&["commit", "--allow-empty", "-m", "alt"]);
        self.index("HEAD", 4);
        self.ok(&["switch", "--detach", back]);
    }
}

#[test]
fn down_and_up_walk_the_stack() {
    let r = Repo::new();
    let out = r.ok(&["down"]);
    assert!(out.contains("two"), "{out}");
    assert_eq!(r.head_subject(), "two");
    r.ok(&["down"]);
    assert_eq!(r.head_subject(), "one");
    r.ok(&["up"]);
    assert_eq!(r.head_subject(), "two");
    r.ok(&["up", "1"]);
    assert_eq!(r.head_subject(), "three");
}

#[test]
fn counts_and_clamping() {
    let r = Repo::new();
    r.ok(&["down", "2"]);
    assert_eq!(r.head_subject(), "one");
    r.ok(&["up", "2"]);
    assert_eq!(r.head_subject(), "three");
    // Past the bottom: moves as far as the stack goes.
    r.ok(&["down", "99"]);
    assert_eq!(r.head_subject(), "one");
    assert!(r.asc(&["up", "-1"]).code != 0);
    assert_eq!(r.asc(&["up", "x"]).code, 129);
}

#[test]
fn top_and_bottom() {
    let r = Repo::new();
    r.ok(&["bottom"]);
    assert_eq!(r.head_subject(), "one");
    r.ok(&["top"]);
    assert_eq!(r.head_subject(), "three");
    assert_eq!(r.asc(&["top", "2"]).code, 129);
}

#[test]
fn aliases_next_and_prev() {
    let r = Repo::new();
    r.ok(&["prev"]);
    assert_eq!(r.head_subject(), "two");
    r.ok(&["next"]);
    assert_eq!(r.head_subject(), "three");
}

#[test]
fn the_ends_are_nothing_to_do() {
    let r = Repo::new();
    let o = r.asc(&["up"]);
    assert_eq!(o.code, 0);
    assert!(o.stderr.contains("top of the stack"), "{}", o.stderr);
    let v: Value = serde_json::from_str(r.ok(&["up", "--json"]).trim()).unwrap();
    assert_eq!(v["status"], "nothing");
    r.ok(&["bottom"]);
    let o = r.asc(&["down"]);
    assert_eq!(o.code, 0);
    assert!(o.stderr.contains("bottom of the stack"), "{}", o.stderr);
    assert_eq!(r.head_subject(), "one");
}

#[test]
fn json_reports_ok() {
    let r = Repo::new();
    let v: Value = serde_json::from_str(r.ok(&["down", "--json"]).trim()).unwrap();
    assert_eq!(v["version"], 1);
    assert_eq!(v["status"], "ok");
    assert_eq!(r.head_subject(), "two");
}

#[test]
fn dry_run_moves_nothing() {
    let r = Repo::new();
    let before = r.sha("HEAD");
    let out = r.ok(&["down", "--dry-run"]);
    assert!(out.contains("two"), "{out}");
    assert_eq!(r.sha("HEAD"), before);
}

#[test]
fn a_fork_exits_2_and_lists_candidates() {
    let r = Repo::new();
    r.fork("feature");
    r.ok(&["bottom"]);
    assert_eq!(r.head_subject(), "one");
    let before = r.sha("HEAD");
    let o = r.asc(&["up"]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert_eq!(r.sha("HEAD"), before, "nothing moves at a fork");
    assert!(
        o.stdout.contains(&change_id(2).to_string()) && o.stdout.contains("two"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(&change_id(4).to_string()) && o.stdout.contains("alt"),
        "{}",
        o.stdout
    );

    let j = r.asc(&["up", "--json"]);
    assert_eq!(j.code, 2);
    let v: Value = serde_json::from_str(j.stdout.trim()).unwrap();
    assert_eq!(v["status"], "ambiguous");
    let subjects: Vec<&str> = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects.len(), 2);
    assert!(subjects.contains(&"two") && subjects.contains(&"alt"));
    assert_eq!(r.sha("HEAD"), before);

    // `top` and `next` stop at the same fork.
    assert_eq!(r.asc(&["top"]).code, 2);
    assert_eq!(r.asc(&["next"]).code, 2);
}

#[test]
fn to_chooses_at_a_fork() {
    let r = Repo::new();
    r.fork("feature");
    r.ok(&["bottom"]);
    r.ok(&["up", "--to", &change_id(4).short()]);
    assert_eq!(r.head_subject(), "alt");
    r.ok(&["bottom"]);
    r.ok(&["up", "--to", "alt"]);
    assert_eq!(r.head_subject(), "alt");
    r.ok(&["bottom"]);
    r.ok(&["top", "--to", &change_id(2).to_string()]);
    assert_eq!(r.head_subject(), "three");
    // A name that is no child leaves the fork unresolved.
    r.ok(&["bottom"]);
    assert_eq!(r.asc(&["up", "--to", "nope"]).code, 2);
}
