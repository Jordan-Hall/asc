//! P46 acceptance: `asc split --hunks <spec>`, driven through the built binary. The change index is
//! filled directly in `state.db`, as in the C13 tests: `feature` holds one < two < three; `two`
//! edits two distant lines of `f` and adds `g`, so it can be split by hunk.

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
        r.write("f", &lines(|_| false));
        r.ok(&["add", "f"]);
        r.ok(&["commit", "-m", "base"]);
        r.ok(&["switch", "-c", "feature"]);
        r.commit(
            &[(
                "one", "one
",
            )],
            "one",
        );
        r.write("f", &lines(|n| n == 2 || n == 28));
        r.write(
            "g", "new
",
        );
        r.ok(&["add", "f", "g"]);
        r.ok(&["commit", "-m", "two"]);
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

    fn change(&self, rev: &str) -> String {
        self.ok(&["change-id", rev]).trim().to_owned()
    }
}

fn lines(changed: impl Fn(u32) -> bool) -> String {
    (1..=30)
        .map(|n| {
            if changed(n) {
                format!(
                    "L{n:02}
"
                )
            } else {
                format!(
                    "l{n:02}
"
                )
            }
        })
        .collect()
}

fn b64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(char::from(T[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The patch of the first hunk of `f` (line 2).
const HUNK_ONE: &str = "diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,5 +1,5 @@
 l01
-l02
+L02
 l03
 l04
 l05
";

impl Repo {
    fn spec(&self, json: &str) -> String {
        let p = self.sb.user.join("spec.json");
        fs::write(&p, json).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn split(&self, pieces: &[(&str, &str, &[&str])]) -> Out {
        let items: Vec<String> = pieces
            .iter()
            .map(|(m, patch, whole)| {
                let w: Vec<String> = whole.iter().map(|w| format!("{w:?}")).collect();
                format!(
                    "{{\"message\":{m:?},\"patch\":\"{}\",\"wholeFiles\":[{}]}}",
                    b64(patch.as_bytes()),
                    w.join(",")
                )
            })
            .collect();
        let spec = self.spec(&format!("{{\"pieces\":[{}]}}", items.join(",")));
        self.asc(&["split", "--change", "feature~1", "--hunks", &spec])
    }

    fn file_at(&self, rev: &str, path: &str) -> String {
        self.ok(&["cat-file", "-p", &format!("{rev}:{path}")])
    }
}

#[test]
fn hunks_split_makes_a_stack_from_a_patch_per_piece() {
    let r = Repo::new();
    let old_head_tree = r.tree("feature");
    let two_tree = r.tree("feature~1");
    let out = r.split(&[("first hunk", HUNK_ONE, &[]), ("", "", &[])]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        r.subjects("feature"),
        ["three", "two", "first hunk", "one", "base"]
    );
    let lower = r.file_at("feature~2", "f");
    assert!(
        lower.contains("L02") && lower.contains("l28") && !lower.contains("L28"),
        "{lower}"
    );
    assert_eq!(r.tree("feature~1"), two_tree);
    assert_eq!(r.tree("feature"), old_head_tree);
    assert_eq!(r.status(), "");
    assert_eq!(r.change("feature~1"), change_id(2).to_string());
    assert_eq!(r.change("feature"), change_id(3).to_string());
    assert_ne!(r.change("feature~2"), change_id(2).to_string());
}

#[test]
fn whole_files_take_the_full_edit_and_the_last_piece_the_rest() {
    let r = Repo::new();
    let out = r.split(&[("the new file", "", &["g"]), ("", "", &[])]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        r.file_at("feature~2", "g"),
        "new
"
    );
    assert_eq!(r.file_at("feature~2", "f"), lines(|_| false));
    assert_eq!(r.file_at("feature~1", "f"), lines(|n| n == 2 || n == 28));
}

#[test]
fn a_bad_spec_changes_nothing() {
    let r = Repo::new();
    let head = r.sha("feature");
    let untouched = "diff --git a/one b/one
--- a/one
+++ b/one
@@ -1 +1 @@
-one
+uno
";
    let stale = "diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 l01
-zzz
+yyy
 l03
";
    let escape = "diff --git a/.git/config b/.git/config
new file mode 100644
--- /dev/null
+++ b/.git/config
@@ -0,0 +1 @@
+x
";
    let binary = "diff --git a/f b/f
GIT binary patch
literal 0
HcmV?d00001
";
    for (name, first) in [
        (
            "a path the change does not touch",
            ("m", untouched, &[][..]),
        ),
        ("a patch that does not apply", ("m", stale, &[][..])),
        ("a .git path", ("m", escape, &[][..])),
        ("a binary patch", ("m", binary, &[][..])),
        (
            "a whole file the change does not touch",
            ("m", "", &["one"][..]),
        ),
        ("a piece that changes nothing", ("m", "", &[][..])),
    ] {
        let o = r.split(&[first, ("", "", &[])]);
        assert_eq!(o.code, 1, "{name}: {}", o.stdout);
        assert_eq!(r.sha("feature"), head, "{name}");
    }
    // One piece, a patch on the last piece, and unknown fields.
    assert_eq!(r.split(&[("m", HUNK_ONE, &[])]).code, 1);
    assert_eq!(
        r.split(&[("m", HUNK_ONE, &[]), ("", HUNK_ONE, &[])]).code,
        1
    );
    let spec = r.spec("{\"pieces\":[{\"x\":1},{}]}");
    assert_eq!(
        r.asc(&["split", "--change", "feature~1", "--hunks", &spec])
            .code,
        1
    );
    // The whole change in the first piece leaves nothing for the last.
    let all = r.split(&[("m", "", &["f", "g"]), ("", "", &[])]);
    assert_eq!(all.code, 1);
    assert_eq!(r.sha("feature"), head);
    // The spec mixes with no other split mode.
    let spec = r.spec("{\"pieces\":[{},{}]}");
    assert_eq!(
        r.asc(&["split", "--hunks", &spec, "--by-file", "f"]).code,
        129
    );
}
