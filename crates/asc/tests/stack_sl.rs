//! C08 acceptance: `asc sl`, `asc stack info|parent|children|trunk` and `asc change-id`, driven
//! through the built binary. The change index is filled directly in `state.db` (the sinks that
//! mint ids on commit belong to C04), so the stacks under test are exactly the rows written here.

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
        ok(&sb, &root, &["commit", "--allow-empty", "-m", "one"]);
        ok(&sb, &root, &["commit", "--allow-empty", "-m", "two"]);
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

    /// Indexes `rev` as change number `n`.
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

    fn stack(&self) {
        self.index("HEAD~1", 1);
        self.index("HEAD", 2);
    }

    fn sl_json(&self, args: &[&str]) -> Value {
        let mut a = vec!["sl", "--json"];
        a.extend_from_slice(args);
        serde_json::from_str(self.ok(&a).trim()).expect("json")
    }
}

#[test]
fn sl_json_is_workspace_stacks() {
    let r = Repo::new();
    r.stack();
    let v = r.sl_json(&[]);
    assert_eq!(v["version"], 1);
    assert_eq!(v["trunks"][0]["name"], "master");
    assert_eq!(v["trunks"][0]["tip"], r.sha("master"));
    assert_eq!(v["trunks"][0]["remote"], Value::Null);
    let stacks = v["stacks"].as_array().unwrap();
    assert_eq!(stacks.len(), 1);
    let s = &stacks[0];
    assert_eq!(s["trunk"], "master");
    assert_eq!(s["base"], r.sha("master"));
    assert_eq!(s["behind"], 0);
    let c = s["changes"].as_array().unwrap();
    assert_eq!(c.len(), 2);
    // Bottom to top.
    assert_eq!(c[0]["subject"], "one");
    assert_eq!(c[1]["subject"], "two");
    assert_eq!(c[0]["change"], change_id(1).to_string());
    assert_eq!(c[0]["short"], change_id(1).short());
    assert_eq!(c[0]["commit"], r.sha("HEAD~1"));
    assert_eq!(c[0]["asc"].as_str().unwrap().len(), 64);
    assert_eq!(c[0]["parent_change"], Value::Null);
    assert_eq!(c[1]["parent_change"], change_id(1).to_string());
    assert_eq!(c[0]["current"], false);
    assert_eq!(c[1]["current"], true);
    assert_eq!(c[1]["branches"][0], "feature");
    assert_eq!(c[1]["state"], "draft");
    assert_eq!(c[1]["needs_restack"], false);
    assert_eq!(c[1]["frozen"], false);
    assert_eq!(c[1]["empty"], true);
    assert_eq!(c[1]["worktree"], Value::Null);
    assert_eq!(c[1]["author"]["name"], "T");
    assert_eq!(c[1]["author"]["email"], "t@example.com");
    assert!(c[1].get("review").is_none());
}

#[test]
fn sl_text_lists_the_stack_top_down_with_the_current_marker() {
    let r = Repo::new();
    r.stack();
    let t = r.ok(&["sl"]);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 3, "{t}");
    assert!(
        lines[0].starts_with(&format!("@  {} two (feature)", change_id(2).short())),
        "{t}"
    );
    assert!(
        lines[1].starts_with(&format!("o  {} one", change_id(1).short())),
        "{t}"
    );
    assert!(lines[2].starts_with("~  on master "), "{t}");
    assert_eq!(r.ok(&["smartlog"]), t);
}

#[test]
fn sl_without_indexed_changes_is_empty() {
    let r = Repo::new();
    assert_eq!(r.sl_json(&[])["stacks"].as_array().unwrap().len(), 0);
    assert_eq!(r.ok(&["sl"]), "");
}

#[test]
fn sl_reports_how_far_the_trunk_moved_ahead() {
    let r = Repo::new();
    r.stack();
    r.ok(&["switch", "master"]);
    r.ok(&["commit", "--allow-empty", "-m", "trunk moves"]);
    r.ok(&["commit", "--allow-empty", "-m", "again"]);
    r.ok(&["switch", "feature"]);
    let v = r.sl_json(&[]);
    assert_eq!(v["stacks"][0]["behind"], 2);
    assert_eq!(v["stacks"][0]["trunk"], "master");
}

#[test]
fn sl_s_shows_only_the_current_stack_and_r_filters() {
    let r = Repo::new();
    r.stack();
    // A second stack on another branch off master.
    r.ok(&["switch", "master"]);
    r.ok(&["switch", "-c", "other"]);
    r.ok(&["commit", "--allow-empty", "-m", "side"]);
    r.index("HEAD", 3);
    let all = r.sl_json(&[]);
    assert_eq!(all["stacks"].as_array().unwrap().len(), 2);
    let cur = r.sl_json(&["-s"]);
    assert_eq!(cur["stacks"].as_array().unwrap().len(), 1);
    assert_eq!(cur["stacks"][0]["changes"][0]["subject"], "side");
    // A branch pointing into the middle of a stack does not list a second, shorter stack.
    r.ok(&["branch", "middle", "feature~1"]);
    assert_eq!(r.sl_json(&[])["stacks"].as_array().unwrap().len(), 2);
    let bad = r.asc(&["sl", "-r", "nonsense("]);
    assert_eq!(bad.code, 129, "{}", bad.stderr);
}

#[test]
fn a_commit_reachable_from_an_immutable_ref_is_not_a_draft() {
    let r = Repo::new();
    r.stack();
    r.ok(&["config", "asc.stack.immutable", "refs/heads/feature"]);
    assert_eq!(r.sl_json(&[])["stacks"].as_array().unwrap().len(), 0);
}

#[test]
fn change_id_prints_the_id_or_exits_one() {
    let r = Repo::new();
    r.stack();
    assert_eq!(r.ok(&["change-id"]), format!("{}\n", change_id(2)));
    assert_eq!(
        r.ok(&["change-id", "--short", "HEAD~1"]),
        format!("{}\n", change_id(1).short())
    );
    let none = r.asc(&["change-id", "master"]);
    assert_eq!(
        (none.code, none.stdout.as_str()),
        (1, ""),
        "{}",
        none.stderr
    );
    let j: Value =
        serde_json::from_str(r.asc(&["change-id", "--json", "master"]).stdout.trim()).unwrap();
    assert_eq!(j["change"], Value::Null);
    let bad = r.asc(&["change-id", "no-such-rev"]);
    assert_eq!(bad.code, 128, "{}", bad.stderr);
}

#[test]
fn stack_parent_children_and_info() {
    let r = Repo::new();
    r.stack();
    let (c1, c2) = (change_id(1).to_string(), change_id(2).to_string());
    assert_eq!(r.ok(&["stack", "parent"]), format!("{c1}\n"));
    assert_eq!(
        r.ok(&["stack", "children", &change_id(1).short()[..6]]),
        format!("{c2}\n")
    );
    // The bottom change has no parent: exit 1, nothing printed.
    let bottom = r.asc(&["stack", "parent", &c1]);
    assert_eq!(
        (bottom.code, bottom.stdout.as_str()),
        (1, ""),
        "{}",
        bottom.stderr
    );
    // The top change has no children: an empty list, exit 0.
    let top = r.asc(&["stack", "children"]);
    assert_eq!((top.code, top.stdout.as_str()), (0, ""), "{}", top.stderr);
    let j: Value =
        serde_json::from_str(r.ok(&["stack", "children", "--json", &c1]).trim()).unwrap();
    assert_eq!(j["version"], 1);
    assert_eq!(j["changes"][0]["change"], c2);

    let info: Value = serde_json::from_str(r.ok(&["stack", "info", "--json"]).trim()).unwrap();
    assert_eq!(info["change"]["change"], c2);
    assert_eq!(info["position"], 2);
    assert_eq!(info["trunk"], "master");
    assert_eq!(info["children"].as_array().unwrap().len(), 0);
    let text = r.ok(&["stack", "info", "HEAD~1"]);
    assert!(text.contains(&format!("change:   {c1}")), "{text}");
    assert!(text.contains("children: 1"), "{text}");
    // On a commit that is not a draft change.
    let off = r.asc(&["stack", "info", "master"]);
    assert_eq!(off.code, 1, "{}", off.stderr);
}

#[test]
fn stack_trunk_reads_and_edits_the_configured_trunks() {
    let r = Repo::new();
    assert_eq!(r.ok(&["stack", "trunk"]), "master\n");
    r.ok(&["stack", "trunk", "--add", "develop"]);
    assert_eq!(r.ok(&["stack", "trunk"]), "master\ndevelop\n");
    r.ok(&["stack", "trunk", "--add", "develop"]);
    assert_eq!(r.ok(&["stack", "trunk"]), "master\ndevelop\n");
    r.ok(&["stack", "trunk", "--remove", "develop"]);
    assert_eq!(r.ok(&["stack", "trunk"]), "master\n");
    let missing = r.asc(&["stack", "trunk", "--remove", "nope"]);
    assert_eq!(missing.code, 128, "{}", missing.stderr);
    let bad = r.asc(&["stack", "trunk", "--add", "a b"]);
    assert_eq!(bad.code, 128, "{}", bad.stderr);
    let j: Value = serde_json::from_str(r.ok(&["stack", "trunk", "--json"]).trim()).unwrap();
    assert_eq!(j["trunks"][0]["name"], "master");
    assert_eq!(j["trunks"][0]["tip"], r.sha("master"));
}

#[test]
fn stack_without_a_known_word_says_so() {
    let r = Repo::new();
    let none = r.asc(&["stack"]);
    assert_eq!(none.code, 129, "{}", none.stderr);
    let unknown = r.asc(&["stack", "nosuchword"]);
    assert_ne!(unknown.code, 0);
    assert!(
        unknown.stderr.contains("not supported yet"),
        "{}",
        unknown.stderr
    );
}

#[test]
fn control_characters_in_a_subject_never_reach_the_terminal() {
    let r = Repo::new();
    r.ok(&["commit", "--allow-empty", "-m", "evil \u{1b}[31mred"]);
    r.index("HEAD", 9);
    let t = r.ok(&["sl"]);
    assert!(!t.contains('\u{1b}'), "{t:?}");
}
