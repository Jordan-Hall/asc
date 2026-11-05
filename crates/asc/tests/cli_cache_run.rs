//! `asc cache run`: a miss runs the command and stores what it wrote; a hit, in the same work
//! tree or another with the same inputs, restores the outputs and replays the output without
//! running anything; a change to an input runs it again.

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use support::{Sandbox, asc_binary, text};

/// Writes `out/f.txt`, counts its runs in `$COUNT`, and prints to both streams.
fn command() -> Vec<&'static str> {
    if cfg!(windows) {
        vec![
            "cmd",
            "/C",
            "(if not exist out mkdir out) & echo built> out\\f.txt & echo ran>> %COUNT% & echo hello & echo warn 1>&2",
        ]
    } else {
        vec![
            "sh",
            "-c",
            "mkdir -p out && echo built > out/f.txt && echo ran >> \"$COUNT\" && echo hello && echo warn >&2",
        ]
    }
}

fn run(sb: &Sandbox, cwd: &Path, count: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .env("COUNT", count)
        .env_remove("CI")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = run(sb, cwd, Path::new("unused"), args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn repo(sb: &Sandbox, name: &str, input: &str) -> PathBuf {
    let repo = sb.dir(name);
    ok(sb, &repo, &["init", "-q"]);
    ok(sb, &repo, &["config", "asc.buildcache.remote", "off"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/in.txt"), input).unwrap();
    std::fs::write(repo.join(".gitignore"), "out/\n").unwrap();
    repo
}

fn cached(sb: &Sandbox, repo: &Path, count: &Path) -> Output {
    let mut args = vec![
        "cache",
        "run",
        "--name",
        "gen",
        "--inputs",
        "src",
        "--outputs",
        "out",
        "--",
    ];
    args.extend(command());
    run(sb, repo, count, &args)
}

fn runs(count: &Path) -> usize {
    std::fs::read_to_string(count).map_or(0, |s| s.lines().count())
}

#[test]
fn a_hit_restores_outputs_and_replays_output_across_work_trees() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let count = sb.dir("counter").join("count.txt");
    let a = repo(&sb, "a", "one\n");

    let miss = cached(&sb, &a, &count);
    assert!(miss.status.success(), "{}", text(&miss.stderr));
    assert_eq!(runs(&count), 1);
    assert!(text(&miss.stdout).contains("hello"));
    assert!(text(&miss.stderr).contains("warn"));

    std::fs::remove_dir_all(a.join("out")).unwrap();
    let hit = cached(&sb, &a, &count);
    assert!(hit.status.success(), "{}", text(&hit.stderr));
    assert_eq!(runs(&count), 1, "a hit runs nothing");
    assert_eq!(
        std::fs::read_to_string(a.join("out/f.txt")).unwrap().trim(),
        "built"
    );
    assert!(text(&hit.stdout).contains("hello"), "stdout replayed");
    let err = text(&hit.stderr);
    assert!(
        err.contains("warn") && err.contains("asc cache: hit gen"),
        "{err}"
    );

    // Another work tree with the same inputs shares the entry.
    let b = repo(&sb, "b", "one\n");
    let shared = cached(&sb, &b, &count);
    assert!(shared.status.success(), "{}", text(&shared.stderr));
    assert_eq!(runs(&count), 1);
    assert!(b.join("out/f.txt").is_file());

    // A changed input is a miss.
    std::fs::write(b.join("src/in.txt"), "two\n").unwrap();
    assert!(cached(&sb, &b, &count).status.success());
    assert_eq!(runs(&count), 2);

    let ls = ok(&sb, &a, &["cache", "ls", "--json"]);
    assert!(ls.contains("\"gen\""), "{ls}");
}

#[test]
fn a_failure_is_not_cached_and_keeps_its_exit_code() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let count = sb.dir("counter").join("count.txt");
    let a = repo(&sb, "a", "one\n");
    let fail: &[&str] = if cfg!(windows) {
        &["cmd", "/C", "echo ran>> %COUNT% & exit 3"]
    } else {
        &["sh", "-c", "echo ran >> \"$COUNT\"; exit 3"]
    };
    let mut args = vec!["cache", "run", "--name", "bad", "--"];
    args.extend(fail);
    for n in 1..=2 {
        let out = run(&sb, &a, &count, &args);
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(runs(&count), n, "a failure runs again");
    }
    let missing = run(
        &sb,
        &a,
        &count,
        &["cache", "run", "--name", "x", "--", "no-such-program-asc"],
    );
    assert!(!missing.status.success());
    assert!(
        text(&missing.stderr).contains("cannot run"),
        "{}",
        text(&missing.stderr)
    );
}
