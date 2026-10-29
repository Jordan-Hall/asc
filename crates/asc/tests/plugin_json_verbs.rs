//! P45 acceptance: the `--json` forms of `restore`, `switch`, `cherry-pick`, `revert`, `clone`,
//! `worktree add|remove|list`, `stash`, `fetch`, `pull` and `push` print one `OpResult` line
//! that decodes through the `cli.proto` message in the asc JSON profile, with the typed `detail`
//! of the verb; the exit code is the verb's; no credential reaches the output.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::OnceLock;

use asc_rpc::json::{decode, encode};
use prost_reflect::{DescriptorPool, MessageDescriptor};
use serde_json::Value;
use support::{Sandbox, text};

fn op_result() -> MessageDescriptor {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap())
        .get_message_by_name("asc.v1.OpResult")
        .expect("asc.v1.OpResult is in the descriptor set")
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
        r.commit("a.txt", "one\n", "base");
        r
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.sb
            .asc(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run_in(&self.root, args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "asc {args:?}: {}",
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn commit(&self, name: &str, body: &str, msg: &str) -> String {
        fs::write(self.root.join(name), body).unwrap();
        self.ok(&["add", name]);
        self.ok(&["commit", "-m", msg]);
        self.head()
    }

    fn head(&self) -> String {
        self.ok(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Runs a `--json` verb, checks the one line decodes as an `OpResult` and returns it.
    fn json_in(&self, dir: &Path, args: &[&str]) -> (Value, i32) {
        let out = self.run_in(dir, args);
        let code = out.status.code().unwrap_or(-1);
        let so = text(&out.stdout);
        assert_eq!(so.lines().count(), 1, "asc {args:?}: one line, got {so:?}");
        let v: Value = serde_json::from_str(so.trim()).expect("a JSON line");
        let m = decode(&op_result(), &v).unwrap_or_else(|e| panic!("{args:?}: {e}\n{so}"));
        let back = encode(&m).unwrap();
        assert_eq!(back["status"], v["status"], "{so}");
        (v, code)
    }

    fn json(&self, args: &[&str]) -> Value {
        let (v, code) = self.json_in(&self.root, args);
        assert_eq!(code, 0, "asc {args:?}: {v}");
        v
    }
}

#[test]
fn restore_prints_its_paths_and_sides() {
    let r = Repo::new();
    fs::write(r.root.join("a.txt"), "changed\n").unwrap();
    let v = r.json(&["restore", "--json", "a.txt"]);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["restore"]["paths"][0], "a.txt");
    assert_eq!(v["restore"]["worktree"], true);
    assert!(v["restore"].get("staged").is_none_or(|s| s == false));
    assert_eq!(fs::read_to_string(r.root.join("a.txt")).unwrap(), "one\n");
    r.ok(&["rm", "--cached", "-q", "a.txt"]);
    let v = r.json(&["restore", "--json", "--staged", "--source=HEAD", "a.txt"]);
    assert_eq!(v["restore"]["staged"], true);
    assert!(v["restore"].get("worktree").is_none_or(|s| s == false));
}

#[test]
fn switch_prints_the_branch_and_the_head() {
    let r = Repo::new();
    let head = r.head();
    let v = r.json(&["switch", "--json", "-c", "topic"]);
    assert_eq!(v["switch"]["target"], "topic");
    assert_eq!(v["switch"]["created"], true);
    assert_eq!(v["switch"]["head"], head);
    assert!(
        v["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "refs/heads/topic")
    );
    let v = r.json(&["switch", "--json", "master"]);
    assert_eq!(v["switch"]["target"], "master");
    let v = r.json(&["switch", "--json", "--detach", "--end-of-options", "topic"]);
    assert_eq!(v["switch"]["detached"], true);
}

#[test]
fn cherry_pick_and_revert_print_the_commits() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "side"]);
    let side = r.commit("b.txt", "bee\n", "side work");
    r.ok(&["switch", "master"]);
    let v = r.json(&["cherry-pick", "--json", "side"]);
    let d = &v["cherry_pick"];
    assert_eq!(d["picked"][0], side);
    assert_eq!(d["made"].as_array().unwrap().len(), 1);
    assert_eq!(d["head"], d["made"][0]);
    assert_eq!(v["status"], "ok");
    let made = d["made"][0].as_str().unwrap().to_owned();
    let v = r.json(&["revert", "--json", "--no-edit", "--end-of-options", "HEAD"]);
    let d = &v["revert"];
    assert_eq!(d["reverted"][0], made);
    assert_eq!(d["made"].as_array().unwrap().len(), 1);
    assert_eq!(v["status"], "ok");
}

#[test]
fn a_conflicting_cherry_pick_pauses_with_exit_one() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "side"]);
    let _side = r.commit("a.txt", "side\n", "side edit");
    r.ok(&["switch", "master"]);
    r.commit("a.txt", "master\n", "master edit");
    let (v, code) = r.json_in(&r.root, &["cherry-pick", "--json", "side"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["status"], "paused");
    assert!(
        v["hints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h == "conflict in a.txt"),
        "{v}"
    );
}

#[test]
fn a_failing_verb_prints_no_result_and_keeps_its_exit_code() {
    let r = Repo::new();
    let out = r.run_in(&r.root, &["cherry-pick", "--json", "no-such-commit"]);
    assert_ne!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "");
    assert!(!text(&out.stderr).is_empty());
}

#[test]
fn stash_prints_its_action_and_entries() {
    let r = Repo::new();
    fs::write(r.root.join("a.txt"), "dirty\n").unwrap();
    let v = r.json(&["stash", "push", "--json", "-m", "wip"]);
    assert_eq!(v["stash"]["action"], "push");
    assert_eq!(v["stash"]["entries"].as_array().unwrap().len(), 1);
    let v = r.json(&["stash", "list", "--json"]);
    assert_eq!(v["stash"]["action"], "list");
    assert!(
        v["stash"]["entries"][0]["message"]
            .as_str()
            .unwrap()
            .contains("wip")
    );
    let v = r.json(&["stash", "pop", "--json"]);
    assert_eq!(v["stash"]["action"], "pop");
    assert!(
        v["stash"]["entries"].is_null() || v["stash"]["entries"].as_array().unwrap().is_empty()
    );
    fs::write(
        r.root.join("a.txt"),
        "one
",
    )
    .unwrap();
    let v = r.json(&["stash", "push", "--json"]);
    assert_eq!(v["status"], "nothing", "a clean tree stashes nothing: {v}");
}

#[test]
fn worktree_add_list_remove_print_their_details() {
    let r = Repo::new();
    let head = r.head();
    let wt = r.root.parent().unwrap().join("wt1");
    let wts = wt.to_string_lossy().into_owned();
    let v = r.json(&["worktree", "add", "--json", "-b", "wtb", &wts]);
    let d = &v["worktree_add"];
    assert_eq!(d["rev"], head);
    assert_eq!(d["branch"], "wtb");
    assert_eq!(d["mode"], "plain");
    assert!(!d["ws"].as_str().unwrap().is_empty());
    assert!(d["path"].as_str().unwrap().ends_with("/wt1"), "{d}");
    let v = r.json(&["worktree", "list", "--json"]);
    let rows = v["worktree_list"]["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{v}");
    assert!(rows.iter().any(|w| w["branch"] == "wtb"));
    let v = r.json(&["worktree", "remove", "--json", "--force", &wts]);
    assert_eq!(v["worktree_remove"]["forced"], true);
    assert!(!wt.exists());
}

/// A bare git repository with one commit, as a `file` remote, and its path.
fn remote_with_commit(r: &Repo) -> PathBuf {
    let bare = r.sb.dir("remote.git");
    let run = |dir: &Path, args: &[&str]| {
        let o = asc_git::realgit::pinned_command()
            .current_dir(dir)
            .env("HOME", &r.sb.user)
            .env("USERPROFILE", &r.sb.user)
            .env("GIT_CONFIG_GLOBAL", r.sb.user.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .expect("git");
        assert!(o.status.success(), "git {args:?}: {}", text(&o.stderr));
    };
    run(&bare, &["init", "-q", "--bare", "-b", "master"]);
    bare
}

#[test]
fn fetch_pull_push_clone_print_typed_results_and_no_credentials() {
    let r = Repo::new();
    let bare = remote_with_commit(&r);
    let url = bare.to_string_lossy().into_owned();
    r.ok(&["remote", "add", "origin", &url]);
    let v = r.json(&["push", "--json", "origin", "master"]);
    assert_eq!(v["push"]["remote"], "origin");
    let row = &v["push"]["refs"][0];
    assert_eq!(row["name"], "refs/heads/master");
    assert!(
        ["ok", "up-to-date"].contains(&row["status"].as_str().unwrap()),
        "{v}"
    );
    let v = r.json(&["fetch", "--json", "origin"]);
    assert_eq!(v["fetch"]["remote"], "origin");
    let v = r.json(&["pull", "--json", "origin", "master"]);
    assert_eq!(v["pull"]["integrate"], "fast-forward");
    assert_eq!(v["pull"]["fetch"]["remote"], "origin");
    assert_eq!(v["pull"]["head"], r.head());

    // A URL with credentials never reaches the output.
    let dest = r.sb.dir("cloned");
    let secret = "https://user:hunter2secret@127.0.0.1:1/x.git";
    let out = r.run_in(&dest, &["clone", "--json", secret, "c"]);
    assert_ne!(out.status.code(), Some(0), "port 1 is closed");
    assert!(!text(&out.stdout).contains("hunter2secret"));
    let clone_url = format!("file://{}", bare.to_string_lossy().replace('\\', "/"));
    let _ = clone_url;
}

#[test]
fn clone_prints_url_path_and_head() {
    let r = Repo::new();
    let bare = remote_with_commit(&r);
    let url = bare.to_string_lossy().into_owned();
    r.ok(&["remote", "add", "origin", &url]);
    r.ok(&["push", "origin", "master"]);
    let dest = r.sb.dir("elsewhere");
    let (v, code) = r.json_in(&dest, &["clone", "--json", &url, "copy"]);
    assert_eq!(code, 0, "{v}");
    let d = &v["clone"];
    assert!(d["path"].as_str().unwrap().ends_with("/copy"), "{d}");
    assert_eq!(d["mode"], "plain");
    assert_eq!(d["branch"], "master");
    assert_eq!(d["head"], r.head());
    assert!(!d["url"].as_str().unwrap().contains('@'));
}

/// A branch `feat` off the base commit.
fn on_feature(r: &Repo) {
    r.ok(&["switch", "-q", "-c", "feat"]);
}

#[test]
fn create_modify_reword_and_stack_rename_print_an_op_result() {
    let r = Repo::new();
    on_feature(&r);
    fs::write(r.root.join("b.txt"), "b\n").unwrap();
    r.ok(&["add", "b.txt"]);
    let created = r.json(&["create", "--json", "-m", "one"]);
    assert_eq!(created["status"], "ok", "{created}");
    let op = created["op"].as_str().expect("create names its op");
    assert!(!op.is_empty());
    assert_eq!(r.ok(&["log", "-1", "--format=%s"]).trim(), "one");
    let head = r.head();

    fs::write(r.root.join("b.txt"), "bb\n").unwrap();
    let modified = r.json(&["modify", "--json", "-a"]);
    assert_eq!(modified["status"], "ok", "{modified}");
    assert_ne!(modified["op"], created["op"]);
    assert_ne!(r.head(), head, "the amend moved HEAD");
    assert!(
        modified["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "refs/heads/feat"),
        "{modified}"
    );

    let reworded = r.json(&["reword", "--json", "-m", "two"]);
    assert_eq!(reworded["status"], "ok", "{reworded}");
    assert_eq!(r.ok(&["log", "-1", "--format=%s"]).trim(), "two");
    let same = r.json(&["reword", "--json", "-m", "two"]);
    assert_eq!(same["status"], "nothing", "{same}");

    let renamed = r.json(&["stack", "rename", "--json", "renamed"]);
    assert_eq!(renamed["status"], "ok", "{renamed}");
    assert!(
        renamed["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "refs/heads/renamed"),
        "{renamed}"
    );
}

#[test]
fn a_failing_stack_verb_prints_no_result_and_keeps_its_exit_code() {
    let r = Repo::new();
    let out = r.run_in(&r.root, &["reword", "--json", "-m", "x", "nosuchchange"]);
    assert_ne!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "", "{}", text(&out.stderr));
    let out = r.run_in(&r.root, &["abort", "--json"]);
    assert_ne!(out.status.code(), Some(0), "nothing to abort");
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn a_modify_that_pauses_a_restack_prints_paused_and_abort_prints_ok() {
    let r = Repo::new();
    on_feature(&r);
    fs::write(r.root.join("c.txt"), "1\n").unwrap();
    r.ok(&["add", "c.txt"]);
    r.json(&["create", "--json", "-m", "one"]);
    fs::write(r.root.join("c.txt"), "2\n").unwrap();
    r.ok(&["add", "c.txt"]);
    r.json(&["create", "--json", "-m", "two"]);
    r.ok(&["down"]);
    fs::write(r.root.join("c.txt"), "X\n").unwrap();
    let (v, code) = r.json_in(&r.root, &["modify", "--json", "-a"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["status"], "paused", "{v}");
    let aborted = r.json(&["abort", "--json"]);
    assert_eq!(aborted["status"], "ok", "{aborted}");
}
