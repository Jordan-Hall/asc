//! A nested worktree (SPEC §9.3) as git tools see it: `rev-parse --show-toplevel` names the
//! requested path, not the mount, and a hook installed under the main workspace's
//! `rev-parse --absolute-git-dir` runs in every work tree, as git's `.git/hooks` does (D-7).

mod support;

use std::fs;
use std::path::Path;
use std::process::Output;

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .env("ASC_NO_DAEMON", "1")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    text(&out.stdout)
}

fn install_hook(dir: &Path, name: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn a_nested_worktree_shows_its_requested_path_and_runs_the_main_hooks() {
    let sb = Sandbox::new();
    let main = sb.dir("repo");
    ok(&sb, &main, &["init", "-q"]);
    fs::write(main.join("a.txt"), "a\n").unwrap();
    ok(&sb, &main, &["add", "a.txt"]);
    ok(&sb, &main, &["commit", "-q", "-m", "base"]);

    let gitdir = ok(&sb, &main, &["rev-parse", "--absolute-git-dir"]);
    install_hook(
        &Path::new(gitdir.trim()).join("hooks"),
        "post-commit",
        "#!/bin/sh\necho fired >> hook-ran\n",
    );

    ok(
        &sb,
        &main,
        &["worktree", "add", "-q", "-b", "x", ".claude/worktrees/x"],
    );
    let wt = main.join(".claude").join("worktrees").join("x");
    let top = ok(&sb, &wt, &["rev-parse", "--show-toplevel"]);
    let top = top.trim();
    assert!(top.ends_with(".claude/worktrees/x"), "{top}");

    fs::write(wt.join("b.txt"), "b\n").unwrap();
    ok(&sb, &wt, &["add", "b.txt"]);
    ok(&sb, &wt, &["commit", "-q", "-m", "b"]);
    assert_eq!(
        fs::read_to_string(wt.join("hook-ran")).unwrap(),
        "fired\n",
        "the hook ran once, at the root of the worktree"
    );

    // The main workspace runs it too.
    fs::write(main.join("c.txt"), "c\n").unwrap();
    ok(&sb, &main, &["add", "c.txt"]);
    ok(&sb, &main, &["commit", "-q", "-m", "c"]);
    assert!(main.join("hook-ran").is_file());
}

#[test]
fn core_hooks_path_replaces_the_main_hooks() {
    let sb = Sandbox::new();
    let main = sb.dir("repo");
    ok(&sb, &main, &["init", "-q"]);
    let gitdir = ok(&sb, &main, &["rev-parse", "--absolute-git-dir"]);
    install_hook(
        &Path::new(gitdir.trim()).join("hooks"),
        "post-commit",
        "#!/bin/sh\necho main >> hook-ran\n",
    );
    install_hook(
        &main.join("my-hooks"),
        "post-commit",
        "#!/bin/sh\necho mine >> hook-ran\n",
    );
    ok(&sb, &main, &["config", "core.hooksPath", "my-hooks"]);
    fs::write(main.join("a.txt"), "a\n").unwrap();
    ok(&sb, &main, &["add", "a.txt"]);
    ok(&sb, &main, &["commit", "-q", "-m", "base"]);
    assert_eq!(fs::read_to_string(main.join("hook-ran")).unwrap(), "mine\n");
}
