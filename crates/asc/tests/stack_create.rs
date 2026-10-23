//! C11 acceptance: `asc create`, `asc modify` and `asc reword`, driven through the built binary.
//! The hooks are a husky-style directory (`core.hooksPath=.husky`) with a commitlint-style
//! `commit-msg` (only `feat: ...` and `fix: ...` subjects pass); each hook appends to a log
//! outside the work tree. A rejecting hook moves no ref; `post-rewrite` runs once per restack
//! with all pairs.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use support::{Sandbox, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

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
    log: PathBuf,
    reject: PathBuf,
}

impl Repo {
    /// `master` has `f`; `feature` is checked out and empty.
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let aux = sb.dir("aux");
        let r = Self {
            log: aux.join("hooks.log"),
            reject: aux.join("reject"),
            sb,
            root,
        };
        r.ok(&["init"]);
        r.write("f", "a\nb\nc\n");
        r.ok(&["add", "f"]);
        r.ok(&["commit", "-m", "base"]);
        r.ok(&["switch", "-c", "feature"]);
        r
    }

    fn slash(p: &Path) -> String {
        p.display().to_string().replace('\\', "/")
    }

    /// Installs the husky fixture.
    fn hooks(&self) {
        let dir = self.root.join(".husky");
        fs::create_dir_all(&dir).unwrap();
        let (log, reject) = (Self::slash(&self.log), Self::slash(&self.reject));
        let scripts = [
            (
                "pre-commit",
                format!(
                    "echo pre-commit >> '{log}'\nif [ -e '{reject}' ]; then echo 'husky: rejected' >&2; exit 1; fi\n"
                ),
            ),
            (
                "commit-msg",
                format!(
                    "echo commit-msg >> '{log}'\ngrep -Eq '^(feat|fix): ' \"$1\" || {{ echo 'commitlint: subject must start with feat: or fix:' >&2; exit 1; }}\n"
                ),
            ),
            ("post-commit", format!("echo post-commit >> '{log}'\n")),
            (
                "post-rewrite",
                format!("echo \"post-rewrite $1 $(wc -l | tr -d ' ')\" >> '{log}'\n"),
            ),
        ];
        for (name, body) in scripts {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            make_executable(&path);
        }
        self.ok(&["config", "core.hooksPath", ".husky"]);
    }

    fn hook_log(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn clear_log(&self) {
        let _ = fs::remove_file(&self.log);
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

    fn branches(&self) -> Vec<String> {
        self.ok(&["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn show(&self, spec: &str) -> String {
        self.ok(&["show", spec])
    }

    fn change(&self, rev: &str) -> String {
        self.ok(&["change-id", rev]).trim().to_owned()
    }

    fn status(&self) -> String {
        self.ok(&["status", "--porcelain"])
            .lines()
            .filter(|l| *l != "?? .ascignore" && !l.starts_with("?? .husky"))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    /// Writes `name` and makes it a change `feat: <name>` with `asc create`.
    fn change_with(&self, name: &str) {
        self.write(name, name);
        self.ok(&["add", name]);
        self.ok(&["create", "-m", &format!("feat: {name}")]);
    }

    /// The stack one < two < three on `feature`.
    fn stack(&self) {
        for n in ["one", "two", "three"] {
            self.change_with(n);
        }
    }
}

#[test]
fn create_runs_the_commit_hooks_and_makes_no_branch() {
    let r = Repo::new();
    r.hooks();
    r.write("one", "one");
    r.ok(&["add", "one"]);
    let before = r.branches();
    r.ok(&["create", "-m", "feat: one"]);
    assert_eq!(r.hook_log(), ["pre-commit", "commit-msg", "post-commit"]);
    assert_eq!(r.subjects("feature"), ["feat: one", "base"]);
    // Branchless: the current branch advanced, nothing new was named.
    assert_eq!(r.branches(), before);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    // The new change has a change id.
    assert!(!r.change("HEAD").is_empty());
    assert_eq!(r.status(), "");
}

#[test]
fn create_with_nothing_staged_makes_an_empty_change() {
    let r = Repo::new();
    r.ok(&["create", "-m", "empty"]);
    assert_eq!(r.subjects("feature"), ["empty", "base"]);
}

#[test]
fn create_names_a_branch_only_when_asked() {
    let r = Repo::new();
    r.write("one", "one");
    r.ok(&["add", "one"]);
    let feature = r.sha("feature");
    r.ok(&["create", "topic", "-m", "feat: one"]);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "topic");
    assert_eq!(r.subjects("topic"), ["feat: one", "base"]);
    // The old branch stayed where it was.
    assert_eq!(r.sha("feature"), feature);
}

#[test]
fn a_rejecting_hook_moves_no_ref() {
    let r = Repo::new();
    r.hooks();
    r.change_with("one");
    let tip = r.sha("feature");
    let branches = r.branches();

    // commitlint rejects the subject.
    r.write("two", "two");
    r.ok(&["add", "two"]);
    let bad = r.asc(&["create", "wip", "-m", "bad subject"]);
    assert_ne!(bad.code, 0);
    assert!(bad.stderr.contains("commitlint"), "{}", bad.stderr);
    assert_eq!(r.sha("feature"), tip);
    assert_eq!(r.branches(), branches);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");

    // husky's pre-commit rejects.
    fs::write(&r.reject, "").unwrap();
    let pre = r.asc(&["create", "-m", "feat: two"]);
    assert_ne!(pre.code, 0);
    assert!(pre.stderr.contains("husky"), "{}", pre.stderr);
    assert_eq!(r.sha("feature"), tip);
    let amend = r.asc(&["modify", "-a"]);
    assert_ne!(amend.code, 0);
    assert_eq!(r.sha("feature"), tip);
    fs::remove_file(&r.reject).unwrap();

    // The staged file is still staged: the same create now works.
    r.ok(&["create", "-m", "feat: two"]);
    assert_eq!(r.subjects("feature"), ["feat: two", "feat: one", "base"]);
}

#[test]
fn modify_amends_the_change_and_restacks_the_descendants_with_one_post_rewrite() {
    let r = Repo::new();
    r.stack();
    let (c1, c2, c3) = (r.change("HEAD~2"), r.change("HEAD~1"), r.change("HEAD"));
    let old_tip = r.sha("feature");
    r.hooks();
    r.ok(&["switch", "--detach", "HEAD~2"]);
    r.write("one", "one-v2");
    r.clear_log();
    let out = r.asc(&["modify", "-a"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stdout
            .lines()
            .filter(|l| l.starts_with("Restacked "))
            .count(),
        2,
        "{}",
        out.stdout
    );
    // The branch carries the amended change and the restacked descendants.
    assert_ne!(r.sha("feature"), old_tip);
    assert_eq!(
        r.subjects("feature"),
        ["feat: three", "feat: two", "feat: one", "base"]
    );
    assert_eq!(r.show("feature~2:one"), "one-v2");
    assert_eq!(r.show("feature:one"), "one-v2");
    assert_eq!(
        (
            r.change("feature~2"),
            r.change("feature~1"),
            r.change("feature")
        ),
        (c1, c2, c3)
    );
    // HEAD is on the amended change, the work tree is clean.
    assert_eq!(r.sha("HEAD"), r.sha("feature~2"));
    assert_eq!(r.status(), "");
    // The amend ran the commit hooks; the restack ran post-rewrite once with both pairs.
    let log = r.hook_log();
    assert!(log.contains(&"pre-commit".to_owned()), "{log:?}");
    assert!(log.contains(&"post-commit".to_owned()), "{log:?}");
    assert_eq!(
        log.iter()
            .filter(|l| l.starts_with("post-rewrite rebase"))
            .collect::<Vec<_>>(),
        ["post-rewrite rebase 2"],
        "{log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|l| l.starts_with("post-rewrite amend"))
            .count(),
        1,
        "{log:?}"
    );
}

#[test]
fn modify_with_c_adds_a_new_change() {
    let r = Repo::new();
    r.change_with("one");
    r.write("two", "two");
    r.ok(&["add", "two"]);
    r.ok(&["modify", "-c", "-m", "feat: two"]);
    assert_eq!(r.subjects("feature"), ["feat: two", "feat: one", "base"]);
}

#[test]
fn modify_into_amends_a_change_below_head_without_checking_it_out() {
    let r = Repo::new();
    r.stack();
    let (c1, c2, c3) = (r.change("HEAD~2"), r.change("HEAD~1"), r.change("HEAD"));
    r.write("one", "one-v2");
    let out = r.asc(&["modify", "--into", &c1, "-a"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        r.subjects("feature"),
        ["feat: three", "feat: two", "feat: one", "base"]
    );
    assert_eq!(r.show("feature~2:one"), "one-v2");
    assert_eq!(r.show("feature:one"), "one-v2");
    assert_eq!(
        (
            r.change("feature~2"),
            r.change("feature~1"),
            r.change("feature")
        ),
        (c1, c2, c3)
    );
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(fs::read_to_string(r.root.join("one")).unwrap(), "one-v2");
    assert_eq!(r.status(), "");
}

#[test]
fn modify_into_a_conflicting_change_changes_nothing() {
    let r = Repo::new();
    r.write("f", "a\nb-one\nc\n");
    r.ok(&["add", "f"]);
    r.ok(&["create", "-m", "feat: one"]);
    r.write("f", "a\nb-two\nc\n");
    r.ok(&["add", "f"]);
    r.ok(&["create", "-m", "feat: two"]);
    let c1 = r.change("HEAD~1");
    let tip = r.sha("feature");
    r.write("f", "a\nb-three\nc\n");
    r.ok(&["add", "f"]);
    let out = r.asc(&["modify", "--into", &c1]);
    assert_ne!(out.code, 0);
    assert_eq!(r.sha("feature"), tip);
    assert_eq!(r.status(), "M  f\n");
}

#[test]
fn reword_edits_the_message_and_restacks() {
    let r = Repo::new();
    r.stack();
    let (c1, c2, c3) = (r.change("HEAD~2"), r.change("HEAD~1"), r.change("HEAD"));
    let tree = r.sha("feature~1^{tree}");
    r.hooks();
    r.clear_log();
    let out = r.asc(&["reword", "HEAD~1", "-m", "fix: second"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        r.subjects("feature"),
        ["feat: three", "fix: second", "feat: one", "base"]
    );
    // The tree of the reworded change is untouched; the change ids are kept.
    assert_eq!(r.sha("feature~1^{tree}"), tree);
    assert_eq!(
        (
            r.change("feature~2"),
            r.change("feature~1"),
            r.change("feature")
        ),
        (c1, c2, c3)
    );
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(r.status(), "");
    let log = r.hook_log();
    assert_eq!(
        log.iter().filter(|l| *l == "commit-msg").count(),
        1,
        "{log:?}"
    );
    assert!(!log.contains(&"pre-commit".to_owned()), "{log:?}");
    assert_eq!(
        log.iter()
            .filter(|l| l.starts_with("post-rewrite rebase"))
            .collect::<Vec<_>>(),
        ["post-rewrite rebase 1"],
        "{log:?}"
    );
}

#[test]
fn reword_rejected_by_commit_msg_moves_no_ref() {
    let r = Repo::new();
    r.stack();
    r.hooks();
    let tip = r.sha("feature");
    let out = r.asc(&["reword", "-m", "no good"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("commitlint"), "{}", out.stderr);
    assert_eq!(r.sha("feature"), tip);
    assert_eq!(
        r.subjects("feature"),
        ["feat: three", "feat: two", "feat: one", "base"]
    );
}

#[test]
fn reword_of_the_tip_moves_the_branch() {
    let r = Repo::new();
    r.change_with("one");
    r.ok(&["reword", "-m", "feat: uno"]);
    assert_eq!(r.subjects("feature"), ["feat: uno", "base"]);
    assert_eq!(r.ok(&["branch", "--show-current"]).trim(), "feature");
}

#[test]
fn create_insert_reparents_the_children() {
    let r = Repo::new();
    r.stack();
    let (c2, c3) = (r.change("HEAD~1"), r.change("HEAD"));
    r.ok(&["switch", "--detach", "HEAD~2"]);
    r.write("mid", "mid");
    r.ok(&["add", "mid"]);
    let out = r.asc(&["create", "--insert", "-m", "feat: mid"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        r.subjects("feature"),
        ["feat: three", "feat: two", "feat: mid", "feat: one", "base"]
    );
    assert_eq!((r.change("feature~1"), r.change("feature")), (c2, c3));
    assert_eq!(r.sha("HEAD"), r.sha("feature~2"));
    assert_eq!(r.show("feature:mid"), "mid");
    assert_eq!(r.status(), "");
}

#[test]
fn create_onto_builds_on_another_revision() {
    let r = Repo::new();
    r.stack();
    r.write("side", "side");
    r.ok(&["add", "side"]);
    r.ok(&["create", "--onto", "feature~2", "-m", "feat: side"]);
    assert_eq!(r.subjects("HEAD"), ["feat: side", "feat: one", "base"]);
    // The stack is untouched.
    assert_eq!(
        r.subjects("feature"),
        ["feat: three", "feat: two", "feat: one", "base"]
    );
}
