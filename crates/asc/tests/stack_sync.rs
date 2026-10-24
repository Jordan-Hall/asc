//! C25 acceptance: `asc sync` on a plain git remote (the Stage 0 fallback), through the built
//! binary. The remote is a bare repository made by real git. The change index is filled
//! directly in `state.db`, as in the C10 tests.

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

fn change_id(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

struct Repo {
    sb: Sandbox,
    root: PathBuf,
    bare: PathBuf,
    other: PathBuf,
}

impl Repo {
    /// `master` has `f`; `origin` is a bare repository holding it; `other` is a second git
    /// clone that plays the server side.
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let bare = sb.dir("remote.git");
        let other = sb.dir("other");
        let r = Self {
            sb,
            root,
            bare,
            other,
        };
        r.git(&r.bare, &["init", "-q", "--bare", "-b", "master"]);
        r.ok(&["init"]);
        r.write("f", "a\nb\nc\n");
        r.ok(&["add", "f"]);
        r.ok(&["commit", "-m", "base"]);
        r.ok(&["remote", "add", "origin", r.bare.to_str().unwrap()]);
        r.ok(&["push", "origin", "master"]);
        let url = r.bare.to_str().unwrap().to_owned();
        r.git(&r.other, &["clone", "-q", &url, "."]);
        r
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .sb
            .command("git", dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    /// A commit on the server side (`other`), pushed to the bare remote.
    fn upstream(&self, file: &str, body: &str, msg: &str) {
        fs::write(self.other.join(file), body).unwrap();
        self.git(&self.other, &["add", file]);
        self.git(&self.other, &["commit", "-q", "-m", msg]);
        self.git(&self.other, &["push", "-q", "origin", "HEAD:master"]);
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

    fn commit_file(&self, name: &str, body: &str, msg: &str) {
        self.write(name, body);
        self.ok(&["add", name]);
        self.ok(&["commit", "-m", msg]);
    }

    fn sha(&self, rev: &str) -> String {
        self.ok(&["rev-parse", rev]).trim().to_owned()
    }

    fn subjects(&self, rev: &str) -> Vec<String> {
        self.ok(&["log", "--format=%s", rev])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn branches(&self) -> Vec<String> {
        self.ok(&["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_owned)
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

    /// `one` < `two` on branches `one-br` < `two-br`, HEAD on `two-br`; both indexed.
    fn stack(&self) {
        self.ok(&["switch", "-c", "one-br"]);
        self.commit_file("one", "one", "one");
        self.index("HEAD", 1);
        self.ok(&["switch", "-c", "two-br"]);
        self.commit_file("two", "two", "two");
        self.index("HEAD", 2);
    }
}

fn json(s: &str) -> Value {
    serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("not json ({e}): {s}"))
}

#[test]
fn a_patch_id_landed_change_is_dropped_and_the_rest_restacks_onto_the_new_trunk() {
    let r = Repo::new();
    r.stack();
    // The server lands `one` as a new commit (a rebase merge) and adds another change.
    r.upstream("one", "one", "one (landed)");
    r.upstream("g", "g", "other");
    let o = r.asc(&["sync"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("Fetched origin"), "{}", o.stdout);
    assert!(o.stdout.contains("Fast-forwarded master"), "{}", o.stdout);
    assert!(
        o.stdout.contains("Dropped") && o.stdout.contains("by patch-id"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("Restacked"), "{}", o.stdout);
    assert_eq!(
        r.subjects("two-br"),
        ["two", "other", "one (landed)", "base"]
    );
    assert_eq!(r.subjects("master"), ["other", "one (landed)", "base"]);
    // `one-br` followed its change to the parent, so it is a merged branch, listed not deleted.
    assert!(o.stdout.contains("Merged branches: one-br"), "{}", o.stdout);
    assert!(r.branches().contains(&"one-br".to_owned()));
    // Nothing is left to do.
    let again = r.asc(&["sync", "--json"]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    let v = json(&again.stdout);
    assert_eq!(v["version"], 1);
    assert_eq!(v["landed"].as_array().unwrap().len(), 0);
    assert_eq!(v["restacked"].as_array().unwrap().len(), 0);
}

#[test]
fn dash_d_deletes_the_merged_branches() {
    let r = Repo::new();
    r.stack();
    r.upstream("one", "one", "one (landed)");
    let o = r.asc(&["sync", "-d"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("Deleted branch one-br"), "{}", o.stdout);
    assert!(!r.branches().contains(&"one-br".to_owned()));
    assert!(r.branches().contains(&"two-br".to_owned()));
    assert_eq!(r.subjects("two-br"), ["two", "one (landed)", "base"]);
}

#[test]
fn dash_d_removes_the_branch_settings_with_the_branch() {
    let r = Repo::new();
    r.stack();
    r.upstream("one", "one", "one (landed)");
    r.ok(&["config", "set", "branch.one-br.remote", "origin"]);
    r.ok(&["config", "set", "branch.one-br.merge", "refs/heads/one-br"]);
    let o = r.asc(&["sync", "-d"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!r.branches().contains(&"one-br".to_owned()));
    let left = r.asc(&["config", "get", "branch.one-br.remote"]);
    assert_ne!(left.code, 0, "the branch section must go: {}", left.stdout);
}

#[test]
fn a_remote_named_like_an_option_is_not_fetched_from() {
    let r = Repo::new();
    r.git(&r.root, &["remote", "remove", "origin"]);
    let url = r.bare.to_str().unwrap().to_owned();
    r.git(
        &r.root,
        &["config", "remote.--upload-pack=touch pwned.url", &url],
    );
    let o = r.asc(&["sync"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains("Fetched"), "{}", o.stdout);
    assert!(!r.root.join("pwned").exists());
}

#[test]
fn dry_run_and_no_fetch_change_nothing() {
    let r = Repo::new();
    r.stack();
    r.upstream("one", "one", "one (landed)");
    let before = (r.sha("two-br"), r.sha("master"), r.branches());
    let o = r.asc(&["sync", "-n"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    // A dry run does not fetch, so the landing is not known yet.
    assert!(!o.stdout.contains("Fetched"), "{}", o.stdout);
    assert_eq!((r.sha("two-br"), r.sha("master"), r.branches()), before);
    let o = r.asc(&["sync", "--no-fetch"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(r.sha("master"), before.1, "--no-fetch must not fetch");
    // With the fetch done by hand, a dry run reports and still changes nothing.
    r.ok(&["fetch", "origin"]);
    let o = r.asc(&["sync", "-n", "--no-fetch"]);
    assert!(
        o.stdout.contains("Would fast-forward master"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("Would drop"), "{}", o.stdout);
    assert_eq!((r.sha("two-br"), r.sha("master"), r.branches()), before);
}

#[test]
fn a_conflicting_stack_is_skipped_reported_and_left_as_it_was() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "feat"]);
    r.commit_file("f", "a\nb-mine\nc\n", "mine");
    r.index("HEAD", 1);
    let before = r.sha("feat");
    r.upstream("f", "a\nb-theirs\nc\n", "theirs");
    let o = r.asc(&["sync"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("Skipped"), "{}", o.stdout);
    assert_eq!(r.sha("feat"), before);
    let git = PathBuf::from(r.ok(&["rev-parse", "--git-dir"]).trim());
    let git = if git.is_absolute() {
        git
    } else {
        r.root.join(git)
    };
    assert!(!git.join("asc-stack-op.json").exists());
    assert!(!git.join("rebase-merge").exists());
    assert_eq!(
        r.ok(&["status", "--porcelain"])
            .lines()
            .filter(|l| *l != "?? .ascignore")
            .count(),
        0
    );
}

#[test]
fn a_diverged_local_trunk_is_not_fast_forwarded() {
    let r = Repo::new();
    r.commit_file("mine", "m", "local only");
    r.upstream("g", "g", "server side");
    let o = r.asc(&["sync"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("diverged"), "{}", o.stdout);
    assert!(!o.stdout.contains("Fast-forwarded"), "{}", o.stdout);
    // The unpushed commit is a draft: it is restacked onto the server's work, never lost.
    assert_eq!(r.subjects("master"), ["local only", "server side", "base"]);
}

#[test]
fn an_empty_change_is_never_matched_by_patch_id() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "empty-br"]);
    r.ok(&["commit", "--allow-empty", "-m", "empty mine"]);
    r.index("HEAD", 1);
    // The server side has an empty commit too: both patches are equally empty.
    r.git(
        &r.other,
        &["commit", "-q", "--allow-empty", "-m", "empty theirs"],
    );
    r.git(&r.other, &["push", "-q", "origin", "HEAD:master"]);
    let o = r.asc(&["sync"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains("Dropped"), "{}", o.stdout);
    assert_eq!(r.subjects("empty-br")[0], "empty mine");
}

#[test]
fn a_fresh_branch_level_with_the_trunk_is_not_a_merged_branch() {
    let r = Repo::new();
    r.ok(&["branch", "fresh"]);
    let o = r.asc(&["sync", "-d"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(r.branches().contains(&"fresh".to_owned()), "{}", o.stdout);
}

#[test]
fn a_squash_merge_is_reported_as_cannot_tell() {
    let r = Repo::new();
    r.ok(&["switch", "-c", "sq"]);
    r.commit_file("a", "a", "a");
    r.commit_file("b", "b", "b");
    r.index("HEAD~1", 1);
    r.index("HEAD", 2);
    r.ok(&["push", "-u", "origin", "sq"]);
    // The server squashes both into one commit and deletes the branch.
    fs::write(r.other.join("a"), "a").unwrap();
    fs::write(r.other.join("b"), "b").unwrap();
    r.git(&r.other, &["add", "a", "b"]);
    r.git(&r.other, &["commit", "-q", "-m", "squashed"]);
    r.git(&r.other, &["push", "-q", "origin", "HEAD:master"]);
    r.git(&r.bare, &["branch", "-D", "sq"]);
    r.ok(&["switch", "master"]);
    let o = r.asc(&["sync", "-d"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("Cannot tell whether 'sq'"),
        "{}",
        o.stdout
    );
    assert!(r.branches().contains(&"sq".to_owned()));
}

#[test]
fn an_operation_in_progress_refuses_sync() {
    let r = Repo::new();
    let git = PathBuf::from(r.ok(&["rev-parse", "--git-dir"]).trim());
    let git = if git.is_absolute() {
        git
    } else {
        r.root.join(git)
    };
    fs::create_dir_all(git.join("rebase-merge")).unwrap();
    let o = r.asc(&["sync"]);
    assert_eq!(o.code, 128, "{}", o.stderr);
    assert!(
        o.stderr.contains("operation is in progress"),
        "{}",
        o.stderr
    );
}

#[test]
fn verbs_that_need_a_server_exit_128_on_a_plain_git_remote() {
    let r = Repo::new();
    let o = r.asc(&["share"]);
    assert_eq!(o.code, 128, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("asc: share needs an asc server (not available on a plain git remote)"),
        "{}",
        o.stderr
    );
    // `submit`, `stack get` and `stack merge` belong to other units; they must not succeed.
    for args in [
        &["submit"][..],
        &[
            "stack",
            "get",
            "asc:0000000000000000000000000000000000000000",
        ],
        &["stack", "merge"],
    ] {
        let o = r.asc(args);
        assert_ne!(o.code, 0, "asc {args:?}: {}", o.stdout);
    }
}
