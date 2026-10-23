//! C07 acceptance: the stack verbs as asc-only verbs (SPEC §11.4, §20.1 D5), driven through the
//! built binary. The registry, the precedence order and the `--json` shapes have unit tests
//! beside their code (`stack/precedence.rs`, `stack/json.rs`, `stack/cli/mod.rs`).

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use support::{Sandbox, asc_binary, text};

fn bin() -> &'static Path {
    Path::new(asc_binary().expect("integration test"))
}

/// A copy (hardlink) of the binary named `git`, so argv[0] selects the git program.
fn git_binary() -> &'static Path {
    static GIT: OnceLock<PathBuf> = OnceLock::new();
    GIT.get_or_init(|| {
        let dir = bin().parent().unwrap().join("asc-test-git-stack");
        fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        let target = dir.join(name);
        if !target.exists() && fs::hard_link(bin(), &target).is_err() && !target.exists() {
            fs::copy(bin(), &target).unwrap();
        }
        target
    })
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut std::process::Command) -> Out {
    let out = cmd.stdin(std::process::Stdio::null()).output().expect("spawn");
    Out { code: out.status.code().unwrap_or(-1), stdout: text(&out.stdout), stderr: text(&out.stderr) }
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    run(sb.asc(cwd).args(args))
}

fn git(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    run(sb.command(git_binary(), cwd).args(args))
}

fn init_repo(sb: &Sandbox) -> PathBuf {
    let root = sb.dir("repo");
    let out = asc(sb, &root, &["init"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    root
}

fn append_global(sb: &Sandbox, text: &str) {
    let path = sb.user.join(".gitconfig");
    let mut cur = fs::read_to_string(&path).unwrap_or_default();
    cur.push_str(text);
    fs::write(path, cur).unwrap();
}

/// Writes an executable `git-<verb>` script that prints `marker`; returns its directory.
fn external(sb: &Sandbox, verb: &str, marker: &str) -> PathBuf {
    let dir = sb.dir("bin");
    let (file, body) = if cfg!(windows) {
        (format!("git-{verb}.cmd"), format!("@echo {marker}\r\n"))
    } else {
        (format!("git-{verb}"), format!("#!/bin/sh\necho {marker}\n"))
    };
    fs::write(dir.join(&file), body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join(&file), fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn with_path(dir: &Path) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap()
}

const VERBS: &[&str] = &[
    "create", "modify", "absorb", "restack", "continue", "abort", "sync", "submit", "sl", "smartlog", "up",
    "down", "top", "bottom", "next", "prev", "reword", "uncommit", "unamend", "abandon", "change-id", "split",
    "squash", "fold", "move", "reorder", "stack", "undo", "redo", "op", "evolog", "interdiff", "share",
];

#[test]
fn stack_verbs_are_asc_verbs_under_both_programs() {
    let sb = Sandbox::new();
    let root = init_repo(&sb);
    for verb in VERBS {
        for out in [asc(&sb, &root, &[verb]), git(&sb, &root, &[verb])] {
            // Never "not a git command": the verb is known. A served verb may still fail
            // on this empty repository (for example "HEAD is not on a change", exit 1).
            assert!(!out.stderr.contains("is not a git command"), "{verb}: {}", out.stderr);
        }
    }
}

#[test]
fn a_git_sync_on_path_wins_over_the_verb_under_git_only() {
    let sb = Sandbox::new();
    let root = init_repo(&sb);
    let dir = external(&sb, "sync", "external-sync");
    let mut cmd = sb.command(git_binary(), &root);
    cmd.env("PATH", with_path(&dir)).arg("sync");
    let out = run(&mut cmd);
    assert_eq!(out.stdout.trim(), "external-sync", "{}", out.stderr);
    // Under `asc` the verb is first: the external is not run.
    let mut cmd = sb.asc(&root);
    cmd.env("PATH", with_path(&dir)).arg("sync");
    let out = run(&mut cmd);
    assert_ne!(out.stdout.trim(), "external-sync");
    assert!(out.stderr.contains("asc: 'sync' is not supported yet") || out.code == 0, "{}", out.stderr);
    // Without the script, `git sync` is the verb.
    let out = git(&sb, &root, &["sync"]);
    assert!(!out.stderr.contains("is not a git command"), "{}", out.stderr);
}

#[test]
fn a_user_alias_wins_over_a_stack_verb_under_git() {
    let sb = Sandbox::new();
    let root = init_repo(&sb);
    append_global(&sb, "[alias]\n\tundo = daemon\n");
    let out = git(&sb, &root, &["undo"]);
    assert_eq!(out.stderr, "fatal: asc: 'daemon' is not supported in an asc workspace\n", "{}", out.stdout);
    // Under `asc` the verb is first.
    let out = asc(&sb, &root, &["undo"]);
    assert!(!out.stderr.contains("daemon"), "{}", out.stderr);
}

#[test]
fn autocorrect_neither_reaches_nor_produces_a_stack_verb() {
    let sb = Sandbox::new();
    let root = init_repo(&sb);
    // A typo of a git command is corrected as in git, to the git command (never a stack verb).
    let out = git(&sb, &root, &["-c", "help.autocorrect=immediate", "bugreprt"]);
    assert!(out.stderr.contains("Continuing under the assumption that you meant 'bugreport'."), "{}", out.stderr);
    // A near miss of a stack verb is not corrected to it, and no suggestion names it.
    for typo in ["creat", "restak", "smartlg", "evolgo"] {
        let out = git(&sb, &root, &["-c", "help.autocorrect=immediate", typo]);
        // Whatever happens (git may autocorrect to one of its own commands), no stack verb is named.
        for verb in VERBS {
            assert!(!out.stderr.contains(&format!("meant '{verb}'")), "{typo}: {}", out.stderr);
            assert!(!out.stderr.contains(&format!("\t{verb}\n")), "{typo}: {}", out.stderr);
        }
    }
    // A stack verb name is exact: it is never autocorrected, even to a git command next to it.
    for verb in ["sl", "up", "op", "fold", "move"] {
        let out = git(&sb, &root, &["-c", "help.autocorrect=immediate", verb]);
        assert!(!out.stderr.contains("WARNING"), "{verb}: {}", out.stderr);
        assert!(!out.stderr.contains("is not a git command"), "{verb}: {}", out.stderr);
    }
}
