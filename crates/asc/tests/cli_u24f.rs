//! U24f acceptance beyond the compat scenarios: the asc-specific behaviour of the remaining
//! plumbing and history verbs (`count-objects`, `ls-remote`, `whatchanged`, `notes`, `mktree`)
//! through the built binary. What can be compared with real git is in the compat scenarios
//! `plumbing-write-*`, `shortlog-basic`, `name-rev-basic`, `range-diff-basic`, `cherry-basic`
//! and `notes-show`.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc_cmd(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Command {
    let mut c = sb.asc(cwd);
    c.envs(IDENT).args(args);
    c
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    asc_cmd(sb, cwd, args).output().expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
}

/// `asc args` with `stdin` piped in.
fn asc_stdin(sb: &Sandbox, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    use std::io::Write as _;
    let mut child = asc_cmd(sb, cwd, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn asc");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn bucket_url(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

fn one_commit(sb: &Sandbox) -> std::path::PathBuf {
    let repo = sb.dir("r");
    ok(sb, &repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    ok(sb, &repo, &["add", "a.txt"]);
    ok(sb, &repo, &["commit", "-q", "-m", "one"]);
    repo
}

#[test]
fn count_objects_prints_git_shaped_zeros() {
    let sb = Sandbox::new();
    let repo = one_commit(&sb);
    assert_eq!(
        text(&ok(&sb, &repo, &["count-objects"]).stdout),
        "0 objects, 0 kilobytes\n"
    );
    assert_eq!(
        text(&ok(&sb, &repo, &["count-objects", "-v"]).stdout),
        "count: 0\nsize: 0\nin-pack: 0\npacks: 0\nsize-pack: 0\nprune-packable: 0\ngarbage: 0\nsize-garbage: 0\n"
    );
    assert_eq!(
        text(&ok(&sb, &repo, &["count-objects", "-vH"]).stdout),
        "count: 0\nsize: 0 bytes\nin-pack: 0\npacks: 0\nsize-pack: 0 bytes\nprune-packable: 0\ngarbage: 0\nsize-garbage: 0 bytes\n"
    );
    let bad = asc(&sb, &repo, &["count-objects", "extra"]);
    assert_eq!(bad.status.code(), Some(129));
}

#[test]
fn ls_remote_lists_the_refs_of_a_configured_remote() {
    let sb = Sandbox::new();
    let repo = one_commit(&sb);
    let url = bucket_url(&sb.dir("bucket"));
    ok(&sb, &repo, &["remote", "add", "origin", &url]);
    ok(&sb, &repo, &["tag", "v1"]);
    ok(&sb, &repo, &["tag", "-a", "-m", "annotated", "v2"]);
    ok(&sb, &repo, &["push", "origin", "master", "v1", "v2"]);

    let head = text(&ok(&sb, &repo, &["rev-parse", "HEAD"]).stdout);
    let head = head.trim();
    let tag2 = text(&ok(&sb, &repo, &["rev-parse", "v2"]).stdout);
    let tag2 = tag2.trim();

    let all = text(&ok(&sb, &repo, &["ls-remote", "origin"]).stdout);
    assert_eq!(
        all,
        format!(
            "{head}\trefs/heads/master\n{head}\trefs/tags/v1\n{tag2}\trefs/tags/v2\n{head}\trefs/tags/v2^{{}}\n"
        )
    );
    let heads = text(&ok(&sb, &repo, &["ls-remote", "--heads", "origin"]).stdout);
    assert_eq!(heads, format!("{head}\trefs/heads/master\n"));
    let refs = text(&ok(&sb, &repo, &["ls-remote", "--tags", "--refs", "origin"]).stdout);
    assert_eq!(
        refs,
        format!("{head}\trefs/tags/v1\n{tag2}\trefs/tags/v2\n")
    );
    let one = text(&ok(&sb, &repo, &["ls-remote", "origin", "v1"]).stdout);
    assert_eq!(one, format!("{head}\trefs/tags/v1\n"));
    let none = asc(
        &sb,
        &repo,
        &["ls-remote", "--exit-code", "origin", "nothing"],
    );
    assert_eq!(none.status.code(), Some(2));
    assert_eq!(
        text(&ok(&sb, &repo, &["ls-remote", "--get-url", "origin"]).stdout),
        format!("{url}\n")
    );
    let missing = asc(&sb, &repo, &["ls-remote", "nosuchremote"]);
    assert_eq!(missing.status.code(), Some(128));
}

#[test]
fn whatchanged_refuses_without_the_acknowledgement() {
    let sb = Sandbox::new();
    let repo = one_commit(&sb);
    let refused = asc(&sb, &repo, &["whatchanged"]);
    assert_eq!(refused.status.code(), Some(128));
    let err = text(&refused.stderr);
    assert!(err.starts_with("'git whatchanged' is nominated for removal."));
    assert!(err.ends_with("fatal: refusing to run without --i-still-use-this\n"));
    let shown = text(
        &ok(
            &sb,
            &repo,
            &["whatchanged", "--i-still-use-this", "--oneline"],
        )
        .stdout,
    );
    assert!(shown.contains(" one\n:000000 100644 "), "{shown}");
    assert!(shown.ends_with(" A\ta.txt\n"), "{shown}");
}

#[test]
fn notes_writes_are_not_supported() {
    let sb = Sandbox::new();
    let repo = one_commit(&sb);
    for sub in ["add", "append", "edit", "copy", "merge", "remove", "prune"] {
        let out = asc(&sb, &repo, &["notes", sub]);
        assert_eq!(out.status.code(), Some(129), "notes {sub}");
        assert_eq!(
            text(&out.stderr),
            "fatal: asc: 'notes' is not supported in an asc workspace\n",
            "notes {sub}"
        );
    }
    assert_eq!(
        text(&ok(&sb, &repo, &["notes", "get-ref"]).stdout),
        "refs/notes/commits\n"
    );
    let none = asc(&sb, &repo, &["notes", "show"]);
    assert_eq!(none.status.code(), Some(1));
    assert!(text(&none.stderr).starts_with("error: no note found for object "));
}

#[test]
fn mktree_refuses_what_a_asc_tree_cannot_hold() {
    let sb = Sandbox::new();
    let repo = one_commit(&sb);
    let blob = text(&ok(&sb, &repo, &["rev-parse", "HEAD:a.txt"]).stdout);
    let blob = blob.trim();
    let dup = format!("100644 blob {blob}\tx\n100644 blob {blob}\tx\n");
    let out = asc_stdin(&sb, &repo, &["mktree"], dup.as_bytes());
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr).contains("duplicate entry 'x'"),
        "{}",
        text(&out.stderr)
    );
    let odd = format!("100664 blob {blob}\tx\n");
    let out = asc_stdin(&sb, &repo, &["mktree"], odd.as_bytes());
    assert_eq!(out.status.code(), Some(128));
    assert!(text(&out.stderr).contains("unsupported mode"));
    let missing = asc_stdin(&sb, &repo, &["mktree", "--missing"], b"");
    assert_eq!(missing.status.code(), Some(129));
}

#[test]
fn commit_tree_and_mktree_agree_with_the_tree_of_a_commit() {
    let sb = Sandbox::new();
    let repo = one_commit(&sb);
    let listing = text(&ok(&sb, &repo, &["ls-tree", "HEAD"]).stdout);
    let rebuilt = asc_stdin(&sb, &repo, &["mktree"], listing.as_bytes());
    assert!(rebuilt.status.success(), "{}", text(&rebuilt.stderr));
    let tree = text(&ok(&sb, &repo, &["rev-parse", "HEAD^{tree}"]).stdout);
    assert_eq!(text(&rebuilt.stdout), tree);
    let commit = ok(&sb, &repo, &["commit-tree", tree.trim(), "-m", "again"]);
    let id = text(&commit.stdout);
    let shown = text(&ok(&sb, &repo, &["cat-file", "-p", id.trim()]).stdout);
    assert!(
        shown.starts_with(&format!("tree {}", tree.trim())),
        "{shown}"
    );
    assert!(shown.ends_with("\nagain\n"), "{shown}");
}
