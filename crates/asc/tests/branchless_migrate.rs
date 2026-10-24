//! B19 acceptance: `asc migrate branchless` (SPEC §21.6.2, client column), driven through the
//! built binary and read back from `state.db`.

mod support;

use std::fs;
use std::path::PathBuf;

use asc_core::change::ChangeId;
use asc_core::change_migrate::import_id;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid};
use asc_state::drafts::Drafts;
use asc_state::state::{git_map, repos, workspaces};
use asc_state::{AscHome, StateDb};
use asc_store::refs::RepoKey;
use support::{Sandbox, text};

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

impl Repo {
    /// `master` with one commit.
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
        r.commit("a.txt", "one\n", "base", None);
        r
    }

    fn run_env(&self, args: &[&str], date: Option<&str>) -> (i32, String, String) {
        let mut cmd = self.sb.asc(&self.root);
        cmd.args(args).stdin(std::process::Stdio::null());
        if let Some(d) = date {
            cmd.env("GIT_COMMITTER_DATE", d).env("GIT_AUTHOR_DATE", d);
        }
        let out = cmd.output().expect("spawn");
        (
            out.status.code().unwrap_or(-1),
            text(&out.stdout),
            text(&out.stderr),
        )
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        self.run_env(args, None)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "asc {args:?}: {err}");
        out
    }

    fn commit(&self, file: &str, body: &str, msg: &str, date: Option<&str>) {
        fs::write(self.root.join(file), body).unwrap();
        self.ok(&["add", file]);
        let (code, _, err) = self.run_env(&["commit", "-m", msg], date);
        assert_eq!(code, 0, "commit: {err}");
    }

    fn branches(&self) -> Vec<String> {
        self.ok(&["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn with_db<T>(&self, f: impl AsyncFnOnce(&StateDb, RepoKey) -> T) -> T {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = AscHome::open(self.sb.asc_home.clone()).unwrap();
            let db = StateDb::open(&home).await.unwrap();
            let ws = workspaces::list(&db, None).await.unwrap().remove(0);
            let repo = repos::get(&db, ws.ws.repo).await.unwrap().unwrap().key;
            f(&db, repo).await
        })
    }

    fn cs_of(&self, rev: &str) -> ChangesetId {
        let sha: GitOid = self.ok(&["rev-parse", rev]).trim().parse().unwrap();
        self.with_db(async |db, repo| {
            ChangesetId::from_bytes(
                git_map::git_to_asc(db, repo, sha, ObjectKind::Commit)
                    .await
                    .unwrap()
                    .unwrap(),
            )
        })
    }

    /// Empties the draft tables, as on a repository from before the sink existed.
    fn clear_rows(&self) {
        self.with_db(async |db, repo| {
            for table in ["draft_heads", "cloud_queue"] {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE repo_id = ?"
                )))
                .bind(repo.0)
                .execute(db.pool())
                .await
                .unwrap();
            }
        });
    }

    fn heads(&self) -> Vec<(ChangesetId, ChangeId)> {
        self.with_db(async |db, repo| {
            let d = Drafts::new(repo);
            let mut v: Vec<_> = d
                .visible(db, 100)
                .await
                .unwrap()
                .into_iter()
                .map(|h| (h.cs, h.change))
                .collect();
            v.sort();
            v
        })
    }

    fn queued(&self) -> usize {
        self.with_db(async |db, repo| Drafts::new(repo).queue_len(db).await.unwrap())
    }

    fn migrate(&self, extra: &[&str]) -> serde_json::Value {
        let mut args = vec!["migrate", "branchless", "--json"];
        args.extend_from_slice(extra);
        let (code, out, err) = self.run(&args);
        assert_eq!(code, 0, "migrate: {err}");
        serde_json::from_str(out.trim()).expect("json")
    }
}

fn names(v: &serde_json::Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

/// `master` + `merged` (same tip), `feat` (one ahead), `old` (one ahead, dated 2020).
fn fixture() -> Repo {
    let r = Repo::new();
    r.ok(&["branch", "merged"]);
    r.ok(&["switch", "-c", "feat"]);
    r.commit("f.txt", "f\n", "feat work", None);
    r.ok(&["switch", "master"]);
    r.ok(&["switch", "-c", "old"]);
    r.commit("o.txt", "o\n", "old work", Some("2020-01-01T00:00:00Z"));
    r.ok(&["switch", "master"]);
    r
}

#[test]
fn reports_merged_and_stale_and_changes_nothing() {
    let r = fixture();
    let before = r.branches();
    let v = r.migrate(&[]);
    assert_eq!(names(&v, "merged"), ["merged"]);
    assert_eq!(names(&v, "stale"), ["old"]);
    assert!(names(&v, "pruned").is_empty());
    assert_eq!(r.branches(), before, "a report deletes nothing");
}

#[test]
fn prune_merged_deletes_only_merged_in_one_recoverable_op() {
    let r = fixture();
    r.ok(&["branch", "merged2"]);
    let v = r.migrate(&["--prune-merged"]);
    assert_eq!(names(&v, "pruned"), ["merged", "merged2"]);
    let left = r.branches();
    for keep in ["master", "feat", "old"] {
        assert!(left.iter().any(|b| b == keep), "{keep} survives: {left:?}");
    }
    assert!(!left.iter().any(|b| b.starts_with("merged")));
    // One op, with the old value of every deleted ref: the op log can put them all back.
    let (ops, rows, with_old) = r.with_db(async |db, repo| {
        let row: (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(DISTINCT op_id), COUNT(*), COUNT(old) FROM ref_log              WHERE repo_id = ? AND new IS NULL AND name LIKE 'refs/heads/merged%'",
        )
        .bind(repo.0)
        .fetch_one(db.pool())
        .await
        .unwrap();
        row
    });
    assert_eq!((ops, rows, with_old), (1, 2, 2));
}

#[test]
fn a_checked_out_merged_branch_is_never_pruned() {
    let r = fixture();
    r.ok(&["switch", "merged"]);
    let v = r.migrate(&["--prune-merged"]);
    assert!(names(&v, "pruned").is_empty());
    assert!(names(&v, "protected").contains(&"merged".to_owned()));
    assert!(r.branches().iter().any(|b| b == "merged"));
}

#[test]
fn master_is_never_pruned_even_when_merged() {
    let r = fixture();
    r.ok(&["branch", "release/1"]);
    let v = r.migrate(&["--prune-merged"]);
    assert!(
        names(&v, "pruned")
            .iter()
            .all(|b| b != "master" && b != "release/1")
    );
    let left = r.branches();
    assert!(left.iter().any(|b| b == "master"));
    assert!(left.iter().any(|b| b == "release/1"));
}

#[test]
fn seeds_draft_heads_from_unmerged_tips_idempotently() {
    let r = fixture();
    r.clear_rows();
    let feat = r.cs_of("feat");
    let old = r.cs_of("old");
    let v = r.migrate(&[]);
    assert_eq!(v["seeded"].as_array().unwrap().len(), 2);
    let heads = r.heads();
    let got: Vec<ChangesetId> = heads.iter().map(|h| h.0).collect();
    assert!(got.contains(&feat) && got.contains(&old), "{got:?}");
    assert_eq!(r.queued(), 2, "seeded heads are queued for upload");

    let again = r.migrate(&[]);
    assert_eq!(again["seeded"].as_array().unwrap().len(), 0);
    assert_eq!(again["already_seeded"], 2);
    assert_eq!(r.heads(), heads, "second run changes nothing");
}

#[test]
fn only_heads_are_seeded_when_branches_stack() {
    let r = fixture();
    r.ok(&["switch", "feat"]);
    r.ok(&["branch", "feat-mid"]);
    r.commit("g.txt", "g\n", "more feat", None);
    r.ok(&["switch", "master"]);
    r.clear_rows();
    let tip = r.cs_of("feat");
    let v = r.migrate(&[]);
    let seeded: Vec<String> = v["seeded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["changeset"].as_str().unwrap().to_owned())
        .collect();
    assert!(seeded.contains(&tip.to_string()));
    assert!(
        !seeded.contains(&r.cs_of("feat-mid").to_string()),
        "feat-mid is an ancestor of feat"
    );
}

#[test]
fn existing_change_id_is_kept_and_a_missing_one_is_derived() {
    let r = fixture();
    let feat = r.cs_of("feat");
    let old = r.cs_of("old");
    // `feat` keeps the id the commit got; `old` loses its row and gets the import-tier id.
    let kept = r.ok(&["change-id", "feat"]).trim().to_owned();
    r.clear_rows();
    r.with_db(async |db, repo| {
        sqlx::query("DELETE FROM changes WHERE repo_id = ? AND cs = ?")
            .bind(repo.0)
            .bind(old.as_bytes().to_vec())
            .execute(db.pool())
            .await
            .unwrap();
    });
    r.migrate(&[]);
    let heads = r.heads();
    let change = |cs: ChangesetId| heads.iter().find(|h| h.0 == cs).unwrap().1;
    assert_eq!(change(feat).to_string(), kept);
    assert_eq!(change(old), import_id(&old));
}

#[test]
fn without_master_nothing_is_classified() {
    let sb = Sandbox::new();
    let root = sb.dir("empty");
    let run = |args: &[&str]| {
        let out = sb.asc(&root).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), text(&out.stderr))
    };
    assert_eq!(run(&["init"]).0, 0);
    let (code, err) = run(&["migrate", "branchless"]);
    assert_ne!(code, 0);
    assert!(err.contains("no master"), "{err}");
}

#[test]
fn unknown_arguments_are_refused() {
    let r = Repo::new();
    let (code, _, err) = r.run(&["migrate", "branchless", "--force"]);
    assert_ne!(code, 0);
    assert!(err.contains("--force"), "{err}");
}
