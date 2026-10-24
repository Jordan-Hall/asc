//! C12 acceptance: `asc absorb`, driven through the built binary. The change index is filled
//! directly in `state.db`, as in the C10 tests: `feature` holds the stack one < two < three
//! (changes 1, 2, 3) over `master`'s `f`; `one` edits line 2, `two` edits line 5 and `three`
//! adds a file.

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

const BASE: &str = "l1\nl2\nl3\nl4\nl5\nl6\n";
const EDITED: &str = "l1\nONE-x\nl3\nl4\nTWO-x\nl6\n";

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
        r.write("f", BASE);
        r.ok(&["add", "f"]);
        r.ok(&["commit", "-m", "base"]);
        r.ok(&["switch", "-c", "feature"]);
        r.write("f", "l1\nONE\nl3\nl4\nl5\nl6\n");
        r.ok(&["commit", "-a", "-m", "one"]);
        r.write("f", "l1\nONE\nl3\nl4\nTWO\nl6\n");
        r.ok(&["commit", "-a", "-m", "two"]);
        r.write("g", "three\n");
        r.ok(&["add", "g"]);
        r.ok(&["commit", "-m", "three"]);
        r.index("HEAD~2", 1);
        r.index("HEAD~1", 2);
        r.index("HEAD", 3);
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

    fn file_at(&self, rev: &str) -> String {
        self.ok(&["show", &format!("{rev}:f")])
    }

    /// `status --porcelain` without the `.ascignore` asc writes at init.
    fn status(&self) -> String {
        self.ok(&["status", "--porcelain"])
            .lines()
            .filter(|l| *l != "?? .ascignore")
            .map(|l| format!("{l}\n"))
            .collect()
    }

    fn change(&self, rev: &str) -> String {
        self.ok(&["change-id", rev]).trim().to_owned()
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
}

#[test]
fn the_default_is_a_preview_that_names_each_owner_and_changes_nothing() {
    let r = Repo::new();
    r.write("f", EDITED);
    let (feature, status) = (r.sha("feature"), r.status());
    let out = r.asc(&["absorb"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stdout.starts_with("Would absorb 2 of 2 hunks"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("one") && out.stdout.contains("two"),
        "{}",
        out.stdout
    );
    assert_eq!((r.sha("feature"), r.status()), (feature, status));

    let v: Value = serde_json::from_str(&r.ok(&["absorb", "-n", "--json"])).unwrap();
    assert_eq!(
        (v["version"].as_u64(), v["status"].as_str()),
        (Some(1), Some("preview"))
    );
    let hunks = v["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 2);
    assert_eq!(
        (hunks[0]["path"].as_str(), hunks[0]["line"].as_u64()),
        (Some("f"), Some(2))
    );
    assert_eq!(
        hunks[0]["change"].as_str(),
        Some(change_id(1).to_string().as_str())
    );
    assert_eq!(
        hunks[1]["change"].as_str(),
        Some(change_id(2).to_string().as_str())
    );
    assert_eq!(
        hunks[1]["commit"].as_str(),
        Some(r.sha("feature~1").as_str())
    );

    let p = r.ok(&["absorb", "-p"]);
    assert!(p.contains("-ONE") && p.contains("+ONE-x"), "{p}");
}

#[test]
fn force_folds_each_hunk_into_its_change_and_restacks() {
    let r = Repo::new();
    let old = [r.sha("feature~2"), r.sha("feature~1"), r.sha("feature")];
    r.write("f", EDITED);
    let out = r.asc(&["absorb", "-f"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stdout.starts_with("Absorbed 2 of 2 hunks"),
        "{}",
        out.stdout
    );
    assert_eq!(r.subjects("feature"), ["three", "two", "one", "base"]);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    for (n, rev) in ["feature~2", "feature~1", "feature"].iter().enumerate() {
        assert_ne!(r.sha(rev), old[n]);
    }
    assert_eq!(r.file_at("feature~2"), "l1\nONE-x\nl3\nl4\nl5\nl6\n");
    assert_eq!(r.file_at("feature~1"), EDITED);
    assert_eq!(r.file_at("feature"), EDITED);
    assert_eq!(r.sha("feature~3"), r.sha("master"));
    // The work tree already had the content; nothing is left to commit.
    assert_eq!(fs::read_to_string(r.root.join("f")).unwrap(), EDITED);
    assert_eq!(r.status(), "");
    // The rewrites keep their change ids.
    assert_eq!(r.change("feature~2"), change_id(1).to_string());
    assert_eq!(r.change("feature~1"), change_id(2).to_string());
    assert_eq!(r.change("feature"), change_id(3).to_string());
    // A second run has nothing to do.
    let again = r.ok(&["absorb", "-f"]);
    assert!(again.starts_with("Nothing to absorb"), "{again}");
}

#[test]
fn json_reports_the_rewrites() {
    let r = Repo::new();
    let old_head = r.sha("feature");
    r.write("f", "l1\nONE\nl3\nl4\nTWO-x\nl6\n");
    let v: Value = serde_json::from_str(&r.ok(&["absorb", "-f", "--json"])).unwrap();
    assert_eq!(v["status"], "applied");
    let rows = v["rewrites"].as_array().unwrap();
    // `one` is below the only target, so `two` and `three` are rewritten.
    assert_eq!(rows.len(), 2, "{v}");
    assert_eq!(rows[1]["subject"], "three");
    assert_eq!(rows[1]["old"], old_head);
    assert_eq!(rows[1]["new"], r.sha("feature"));
}

#[test]
fn hunks_of_immutable_lines_stay_and_stay_modified() {
    let r = Repo::new();
    let feature = r.sha("feature");
    // Line 1 belongs to `base` on master, which is the trunk.
    r.write("f", "L1\nONE\nl3\nl4\nTWO\nl6\n");
    let out = r.ok(&["absorb", "-f"]);
    assert!(
        out.contains("Staying where they are") && out.contains("not-a-candidate"),
        "{out}"
    );
    assert_eq!(r.sha("feature"), feature);
    assert_eq!(r.status(), " M f\n");
}

#[test]
fn a_hunk_over_lines_of_two_changes_is_ambiguous_and_new_files_are_skipped() {
    let r = Repo::new();
    let feature = r.sha("feature");
    r.write("f", "l1\nBOTH\nl6\n");
    r.write("new.txt", "new\n");
    r.ok(&["add", "-N", "new.txt"]);
    let v: Value = serde_json::from_str(&r.ok(&["absorb", "-f", "--json"])).unwrap();
    assert_eq!(v["status"], "nothing", "{v}");
    assert_eq!(v["hunks"][0]["reason"], "ambiguous", "{v}");
    assert_eq!(r.sha("feature"), feature);
}

#[test]
fn staged_hunks_win_over_unstaged_ones() {
    let r = Repo::new();
    r.write("f", "l1\nONE-x\nl3\nl4\nTWO\nl6\n");
    r.ok(&["add", "f"]);
    // An unstaged edit on top is not absorbed.
    r.write("f", "l1\nONE-x\nl3\nl4\nTWO-y\nl6\n");
    r.ok(&["absorb", "-f"]);
    assert_eq!(r.file_at("feature~2"), "l1\nONE-x\nl3\nl4\nl5\nl6\n");
    assert_eq!(r.file_at("feature"), "l1\nONE-x\nl3\nl4\nTWO\nl6\n");
    assert_eq!(r.status(), " M f\n");
    assert_eq!(
        fs::read_to_string(r.root.join("f")).unwrap(),
        "l1\nONE-x\nl3\nl4\nTWO-y\nl6\n"
    );
}

#[test]
fn into_restricts_the_changes_that_may_take_hunks() {
    let r = Repo::new();
    r.write("f", EDITED);
    r.ok(&["branch", "mid", "feature~1"]);
    let spec = "git(mid)".to_owned();
    let v: Value = serde_json::from_str(&r.ok(&["absorb", "--into", &spec, "--json"])).unwrap();
    let hunks = v["hunks"].as_array().unwrap();
    assert_eq!(hunks[0]["reason"], "not-a-candidate");
    assert_eq!(
        hunks[1]["change"].as_str(),
        Some(change_id(2).to_string().as_str())
    );
    let bad = r.asc(&["absorb", "--into", "nosuchfn()"]);
    assert_ne!(bad.code, 0);
}

#[test]
fn a_conflict_with_a_change_above_leaves_the_hunk_and_moves_nothing() {
    let r = Repo::new();
    // A fourth change deletes the line after `one`'s, next to the line `one` wrote.
    r.write("f", "l1\nONE\nl4\nTWO\nl6\n");
    r.ok(&["commit", "-a", "-m", "four"]);
    r.index("HEAD", 4);
    let feature = r.sha("feature");
    r.write("f", "l1\nONE-x\nl4\nTWO\nl6\n");
    let out = r.ok(&["absorb", "-f"]);
    assert!(out.contains("conflict"), "{out}");
    assert_eq!(r.sha("feature"), feature);
    assert_eq!(r.status(), " M f\n");
}

#[test]
fn off_a_stack_there_is_nothing_to_do_and_the_flags_exclude_each_other() {
    let r = Repo::new();
    r.ok(&["switch", "master"]);
    r.write("f", "L1\nl2\nl3\nl4\nl5\nl6\n");
    let out = r.asc(&["absorb", "-f", "--json"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("not a mutable change"),
        "{}",
        out.stderr
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out.stdout).unwrap()["status"],
        "nothing"
    );
    assert_ne!(r.asc(&["absorb", "-n", "-f"]).code, 0);
}

#[test]
fn a_detached_head_follows_its_rewrite() {
    let r = Repo::new();
    r.ok(&["switch", "--detach", "feature"]);
    let before = r.sha("HEAD");
    r.write("f", EDITED);
    r.ok(&["absorb", "-f"]);
    assert_ne!(r.sha("HEAD"), before);
    assert_eq!(r.sha("HEAD"), r.sha("feature"));
    assert_eq!(r.file_at("HEAD~2"), "l1\nONE-x\nl3\nl4\nl5\nl6\n");
    assert_eq!(r.status(), "");
}

/// Points `gpg.program` at a script and turns signing on.
fn sign_with(r: &Repo, name: &str, body: &str) {
    let script = r.sb.tmp.path().join(name);
    fs::write(&script, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    r.ok(&[
        "config",
        "gpg.program",
        &script.display().to_string().replace('\\', "/"),
    ]);
    r.ok(&["config", "user.signingkey", "KEY1"]);
    r.ok(&["config", "commit.gpgsign", "true"]);
}

#[test]
fn every_rewritten_commit_is_signed_when_commit_gpgsign_is_set() {
    let r = Repo::new();
    sign_with(
        &r,
        "fakegpg.sh",
        "#!/bin/sh\ncat > /dev/null\necho '[GNUPG:] SIG_CREATED D 1 8 00 1700000000 ABCDEF' >&2\n\
         printf -- '-----BEGIN PGP SIGNATURE-----\\n\\nZmFrZQ==\\n-----END PGP SIGNATURE-----\\n'\n",
    );
    r.write("f", EDITED);
    r.ok(&["absorb", "-f"]);
    for rev in ["feature", "feature~1", "feature~2"] {
        assert!(
            r.ok(&["cat-file", "-p", rev]).contains("gpgsig"),
            "{rev} is unsigned"
        );
    }
}

#[test]
fn a_failing_signer_moves_no_ref_and_exits_1() {
    let r = Repo::new();
    let head = r.sha("feature");
    sign_with(&r, "badgpg.sh", "#!/bin/sh\ncat > /dev/null\nexit 1\n");
    r.write("f", EDITED);
    let out = r.asc(&["absorb", "-f"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("signing failed"), "{}", out.stderr);
    assert_eq!(r.sha("feature"), head);
    assert_eq!(r.status(), " M f\n");
}
