//! C10 acceptance: `asc restack`, `asc continue` and `asc abort`, driven through the built binary.
//! The change index is filled directly in `state.db`, as in the C09 tests: `feature` holds the
//! stack one < two < three (changes 1, 2, 3) and `amended` holds a rewrite of `one` (change 1,
//! predecessor `one`), so `two` has an obsolete parent.

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

/// Marks a test script executable (Unix runs a script only with the execute bit).
fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

impl Repo {
    /// `master` has `f`; `feature` is one < two < three, checked out. With `conflict`, `two`
    /// and the rewrite of `one` both edit the same line of `f`.
    fn new(conflict: bool) -> Self {
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
        r.write("one", "one");
        r.ok(&["add", "one"]);
        r.ok(&["commit", "-m", "one"]);
        let one = r.sha("HEAD");
        if conflict {
            r.write("f", "a\nb-two\nc\n");
            r.ok(&["add", "f"]);
        }
        r.write("two", "two");
        r.ok(&["add", "two"]);
        r.ok(&["commit", "-m", "two"]);
        r.write("three", "three");
        r.ok(&["add", "three"]);
        r.ok(&["commit", "-m", "three"]);
        r.index("HEAD~2", 1, &[]);
        r.index("HEAD~1", 2, &[]);
        r.index("HEAD", 3, &[]);
        r.ok(&["switch", "master"]);
        r.ok(&["switch", "-c", "amended"]);
        r.write("one", "one-v2");
        r.ok(&["add", "one"]);
        if conflict {
            r.write("f", "a\nb-one\nc\n");
            r.ok(&["add", "f"]);
        }
        r.ok(&["commit", "-m", "one v2"]);
        r.index("HEAD", 1, &[&one]);
        r.ok(&["switch", "feature"]);
        r
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

    fn subjects(&self, rev: &str) -> Vec<String> {
        self.ok(&["log", "--format=%s", rev])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn gitdir(&self) -> PathBuf {
        let dir = PathBuf::from(self.ok(&["rev-parse", "--git-dir"]).trim());
        if dir.is_absolute() {
            dir
        } else {
            self.root.join(dir)
        }
    }

    /// `status --porcelain` without the `.ascignore` asc writes at init.
    fn status(&self) -> String {
        self.ok(&["status", "--porcelain"])
            .lines()
            .filter(|l| *l != "?? .ascignore")
            .map(|l| {
                format!(
                    "{l}
"
                )
            })
            .collect()
    }

    fn index(&self, rev: &str, n: u8, preds: &[&str]) {
        let sha = self.sha(rev);
        let preds: Vec<String> = preds.iter().map(|s| (*s).to_owned()).collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let cs_of = |s: String| {
                let db = db.clone();
                let repo = ws.ws.repo;
                async move {
                    let oid: GitOid = s.parse().unwrap();
                    ChangesetId::from_bytes(
                        git_map::git_to_asc(&db, repo, oid, ObjectKind::Commit)
                            .await
                            .unwrap()
                            .unwrap(),
                    )
                }
            };
            let cs = cs_of(sha).await;
            let mut pred_ids = Vec::new();
            for p in preds {
                pred_ids.push(cs_of(p).await);
            }
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
            rows.insert(&row, &pred_ids).await.unwrap();
        });
    }

    /// The change id (as `asc change-id` prints it) of a revision, if it has one.
    fn change(&self, rev: &str) -> String {
        self.ok(&["change-id", rev]).trim().to_owned()
    }
}

#[test]
fn restack_moves_the_stack_onto_the_rewritten_parent_and_keeps_the_change_ids() {
    let r = Repo::new(false);
    let old_head = r.sha("feature");
    let amended = r.sha("amended");
    let out = r.asc(&["restack"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 2, "{}", out.stdout);
    assert!(
        out.stdout.lines().all(|l| l.starts_with("Restacked ")),
        "{}",
        out.stdout
    );
    assert_ne!(r.sha("feature"), old_head);
    assert_eq!(r.sha("feature~2"), amended);
    assert_eq!(r.subjects("feature"), ["three", "two", "one v2", "base"]);
    // HEAD stayed on the branch, the work tree has the new base and is clean.
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(fs::read_to_string(r.root.join("one")).unwrap(), "one-v2");
    assert_eq!(r.status(), "");
    // The rewrites keep their change ids; `amended` did not move.
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_eq!(r.change("feature~1"), change_id(2).to_string());
    assert_eq!(r.sha("amended"), amended);
    assert!(!r.gitdir().join("asc-stack-op.json").exists());
    // Nothing is left to do.
    let again = r.asc(&["restack", "--json"]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    let v: Value = serde_json::from_str(&again.stdout).unwrap();
    assert_eq!(
        (v["version"].as_u64(), v["status"].as_str()),
        (Some(1), Some("nothing"))
    );
}

#[test]
fn json_reports_the_rewrites() {
    let r = Repo::new(false);
    let old_head = r.sha("feature");
    let v: Value = serde_json::from_str(&r.ok(&["restack", "--json"])).unwrap();
    assert_eq!(v["status"], "complete");
    let rows = v["rewrites"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["subject"], "three");
    assert_eq!(rows[1]["old"], old_head);
    assert_eq!(rows[1]["new"], r.sha("feature"));
}

#[test]
fn scope_flags_exclude_each_other_and_only_a_settled_change_does_nothing() {
    let r = Repo::new(false);
    assert_eq!(r.asc(&["restack", "--only", "--upstack"]).code, 129);
    assert_eq!(r.asc(&["restack", "--downstack", "--only"]).code, 129);
    let head = r.sha("feature");
    // `three` sits on `two`, which is not obsolete: nothing to do for it alone.
    let o = r.asc(&["restack", "--only"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{}", o.stderr);
    assert_eq!(r.sha("feature"), head);
    let o = r.asc(&["restack", "--downstack"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_ne!(r.sha("feature"), head);
}

#[test]
fn a_named_change_must_be_on_a_stack() {
    let r = Repo::new(false);
    assert_eq!(r.asc(&["restack", "master"]).code, 1);
}

#[test]
fn a_conflict_pauses_in_gits_rebase_state_and_continue_finishes() {
    let r = Repo::new(true);
    let old_head = r.sha("feature");
    let out = r.asc(&["restack"]);
    assert_eq!(out.code, 1, "{} {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("restack stopped"), "{}", out.stderr);
    assert!(r.gitdir().join("rebase-merge").is_dir());
    assert!(r.gitdir().join("asc-stack-op.json").is_file());
    // No ref moved while paused.
    assert_eq!(r.sha("refs/heads/feature"), old_head);
    assert!(r.status().contains("UU f"), "{}", r.status());
    // A second restack and an unresolved continue are refused.
    assert_eq!(r.asc(&["restack"]).code, 128);
    let still = r.asc(&["continue"]);
    assert_eq!(still.code, 1, "{}", still.stderr);
    assert!(r.gitdir().join("asc-stack-op.json").is_file());

    r.write("f", "a\nb-both\nc\n");
    r.ok(&["add", "f"]);
    let done = r.asc(&["continue"]);
    assert_eq!(done.code, 0, "{} {}", done.stdout, done.stderr);
    assert!(!r.gitdir().join("rebase-merge").exists());
    assert!(!r.gitdir().join("asc-stack-op.json").exists());
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(r.subjects("feature"), ["three", "two", "one v2", "base"]);
    assert_ne!(r.sha("feature"), old_head);
    assert_eq!(
        fs::read_to_string(r.root.join("f")).unwrap(),
        "a\nb-both\nc\n"
    );
    assert_eq!(fs::read_to_string(r.root.join("three")).unwrap(), "three");
    assert_eq!(r.status(), "");
    assert_eq!(r.change("feature~1"), change_id(2).to_string());
}

#[test]
fn abort_restores_the_before_state() {
    let r = Repo::new(true);
    let old_head = r.sha("feature");
    assert_eq!(r.asc(&["restack"]).code, 1);
    let out = r.asc(&["abort"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.sha("feature"), old_head);
    assert_eq!(r.sha("HEAD"), old_head);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert!(!r.gitdir().join("rebase-merge").exists());
    assert!(!r.gitdir().join("asc-stack-op.json").exists());
    assert_eq!(
        fs::read_to_string(r.root.join("f")).unwrap(),
        "a\nb-two\nc\n"
    );
    assert_eq!(r.status(), "");
    // The stack still needs a restack, and it can be started again.
    assert_eq!(r.asc(&["restack"]).code, 1);
}

#[test]
fn abort_after_git_finished_the_rebase_itself_still_restores() {
    let r = Repo::new(true);
    let old_head = r.sha("feature");
    assert_eq!(r.asc(&["restack"]).code, 1);
    r.ok(&["rebase", "--abort"]);
    assert_eq!(r.asc(&["abort"]).code, 0);
    assert_eq!(r.sha("HEAD"), old_head);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert!(!r.gitdir().join("asc-stack-op.json").exists());
}

#[test]
fn continue_and_abort_without_an_operation_exit_1() {
    let r = Repo::new(false);
    for verb in ["continue", "abort"] {
        let o = r.asc(&[verb]);
        assert_eq!(o.code, 1, "{verb}: {}", o.stderr);
        assert!(
            o.stderr.contains("no operation in progress"),
            "{}",
            o.stderr
        );
    }
}

#[test]
fn continue_and_abort_serve_a_plain_git_operation() {
    let r = Repo::new(true);
    // `feature` and `amended` conflict on `f`.
    let m = r.asc(&["merge", "amended"]);
    assert_eq!(m.code, 1, "{}", m.stderr);
    assert_eq!(r.asc(&["abort"]).code, 0);
    assert!(!r.gitdir().join("MERGE_HEAD").exists());
    assert_eq!(r.status(), "");
}

#[test]
fn a_damaged_state_file_is_refused_and_changes_nothing() {
    let r = Repo::new(true);
    let head = r.sha("feature");
    fs::write(
        r.gitdir().join("asc-stack-op.json"),
        b"{\"version\":1,\"nonsense\":true}",
    )
    .unwrap();
    for verb in ["continue", "abort"] {
        let o = r.asc(&[verb]);
        assert_eq!(o.code, 128, "{verb}: {}", o.stderr);
        assert!(o.stderr.contains("asc-stack-op.json"), "{}", o.stderr);
    }
    assert_eq!(r.asc(&["restack"]).code, 128);
    assert_eq!(r.sha("feature"), head);
}

#[test]
fn a_dirty_work_tree_refuses_a_conflicting_restack_before_anything_moves() {
    let r = Repo::new(true);
    let head = r.sha("feature");
    r.write("three", "edited");
    let o = r.asc(&["restack"]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("uncommitted changes"), "{}", o.stderr);
    assert_eq!(r.sha("feature"), head);
    assert!(!r.gitdir().join("asc-stack-op.json").exists());
    assert!(!r.gitdir().join("rebase-merge").exists());
    assert_eq!(fs::read_to_string(r.root.join("three")).unwrap(), "edited");
}

#[test]
fn a_dirty_work_tree_refuses_a_clean_restack_that_moves_head() {
    let r = Repo::new(false);
    let head = r.sha("feature");
    r.write("three", "edited");
    let o = r.asc(&["restack"]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert_eq!(r.sha("feature"), head);
    assert_eq!(fs::read_to_string(r.root.join("three")).unwrap(), "edited");
}

#[test]
fn a_detached_head_follows_its_rewrite() {
    let r = Repo::new(false);
    r.ok(&["switch", "--detach", "feature"]);
    let out = r.asc(&["restack"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(r.sha("HEAD"), r.sha("feature"));
    assert_eq!(r.subjects("HEAD"), ["three", "two", "one v2", "base"]);
}

#[test]
fn every_rewritten_commit_is_signed_when_commit_gpgsign_is_set() {
    let r = Repo::new(false);
    let script = r.sb.tmp.path().join("fakegpg.sh");
    let body = "#!/bin/sh\ncat > /dev/null\n\
                echo '[GNUPG:] SIG_CREATED D 1 8 00 1700000000 ABCDEF' >&2\n\
                printf -- '-----BEGIN PGP SIGNATURE-----\\n\\nZmFrZQ==\\n-----END PGP SIGNATURE-----\\n'\n";
    fs::write(&script, body).unwrap();
    make_executable(&script);
    let program = script.display().to_string().replace('\\', "/");
    r.ok(&["config", "gpg.program", &program]);
    r.ok(&["config", "user.signingkey", "KEY1"]);
    r.ok(&["config", "commit.gpgsign", "true"]);
    let out = r.asc(&["restack"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    for rev in ["feature", "feature~1"] {
        assert!(
            r.ok(&["cat-file", "-p", rev]).contains("gpgsig"),
            "{rev} is unsigned"
        );
    }
}

#[test]
fn a_failing_signer_moves_no_ref_and_exits_1() {
    let r = Repo::new(false);
    let head = r.sha("feature");
    let script = r.sb.tmp.path().join("badgpg.sh");
    fs::write(&script, "#!/bin/sh\ncat > /dev/null\nexit 1\n").unwrap();
    make_executable(&script);
    r.ok(&[
        "config",
        "gpg.program",
        &script.display().to_string().replace('\\', "/"),
    ]);
    r.ok(&["config", "user.signingkey", "KEY1"]);
    r.ok(&["config", "commit.gpgsign", "true"]);
    let out = r.asc(&["restack"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("signing failed"), "{}", out.stderr);
    assert_eq!(r.sha("feature"), head);
    assert!(!r.gitdir().join("asc-stack-op.json").exists());
}
