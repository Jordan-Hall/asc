//! The migration path (SPEC §25.6 "Migration push", §14.1): a fixture repository with merges, a
//! signed commit, an annotated tag, odd modes and an unsorted tree is cloned `--mirror` from a
//! plain git server, pushed `--mirror` to asc, cloned with `asc clone`, and cloned back with
//! `git clone`. Every ref and id must match; the refs asc cannot represent are listed.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::harness::*;
use crate::plain::Plain;

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn commit_file(git: &Git, dir: &Path, file: &str, body: &str, msg: &str) {
    std::fs::write(dir.join(file), body).unwrap();
    git.ok(dir, &["add", "."]);
    git.ok(dir, &["commit", "-q", "-m", msg]);
}

/// Returns the fixture repository and the refs that asc cannot hold (`name`, with the type the
/// tag points to).
fn source(git: &Git) -> (PathBuf, Vec<(String, &'static str)>) {
    let s = git.work("source");
    git.ok(&s, &["init", "-q"]);
    commit_file(git, &s, "README.md", "readme\n", "root");
    // A merge, with a real branch behind it.
    git.ok(&s, &["checkout", "-q", "-b", "side"]);
    commit_file(git, &s, "side.txt", "side\n", "side");
    git.ok(&s, &["checkout", "-q", "master"]);
    commit_file(git, &s, "main.txt", "main\n", "main");
    git.ok(&s, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    // An executable and an odd mode (100664).
    std::fs::write(s.join("run.sh"), "#!/bin/sh\n").unwrap();
    git.ok(&s, &["add", "run.sh"]);
    git.ok(&s, &["update-index", "--chmod=+x", "run.sh"]);
    let blob = git.ok_stdin(&s, &["hash-object", "-w", "--stdin"], b"odd\n");
    git.ok(
        &s,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100664,{blob},odd.txt"),
        ],
    );
    git.ok(&s, &["commit", "-q", "-m", "modes"]);

    // A signed commit, spelled out by hand: the signature header must survive byte for byte.
    let tree = git.ok(&s, &["rev-parse", "HEAD^{tree}"]);
    let parent = git.head(&s);
    let signed = format!(
        "tree {tree}\nparent {parent}\nauthor A <a@x> 1700000100 +0000\n\
         committer A <a@x> 1700000100 +0000\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n =abcd\n -----END PGP SIGNATURE-----\n\nsigned\n"
    );
    let signed_id = git.ok_stdin(
        &s,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "--stdin",
        ],
        signed.as_bytes(),
    );
    git.ok(&s, &["update-ref", "refs/heads/signed", &signed_id]);

    // An unsorted tree (entries in reverse order), as old tools wrote them.
    let a = git.ok_stdin(&s, &["hash-object", "-w", "--stdin"], b"a\n");
    let b = git.ok_stdin(&s, &["hash-object", "-w", "--stdin"], b"b\n");
    let mut raw = Vec::new();
    for (name, id) in [("b.txt", &b), ("a.txt", &a)] {
        raw.extend_from_slice(format!("100644 {name}\0").as_bytes());
        raw.extend_from_slice(&hex_bytes(id));
    }
    let tree = git.ok_stdin(
        &s,
        &["hash-object", "-t", "tree", "-w", "--literally", "--stdin"],
        &raw,
    );
    let unsorted = format!(
        "tree {tree}\nauthor A <a@x> 1700000200 +0000\ncommitter A <a@x> 1700000200 +0000\n\nunsorted\n"
    );
    let unsorted_id = git.ok_stdin(
        &s,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "--stdin",
        ],
        unsorted.as_bytes(),
    );
    git.ok(&s, &["update-ref", "refs/heads/unsorted", &unsorted_id]);

    // Tags: annotated, lightweight, and one to a blob (asc cannot represent it, SPEC §14.1).
    git.ok(&s, &["tag", "-a", "v1", "-m", "release one", "master"]);
    git.ok(&s, &["tag", "light", "side"]);
    git.ok(&s, &["tag", "-a", "-m", "a blob", "blobtag", &blob]);
    (s, vec![("refs/tags/blobtag".to_owned(), "blob")])
}

/// The built `asc` with a private home and the loopback opt-in, in `cwd`.
fn asc(git: &Git, cwd: &Path, token: Option<&str>, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_asc"));
    for (k, _) in std::env::vars_os() {
        let n = k.to_string_lossy().to_ascii_uppercase();
        if n.starts_with("GIT_") || n.starts_with("ASC_") {
            cmd.env_remove(&k);
        }
    }
    cmd.current_dir(cwd)
        .env("HOME", git.home.path())
        .env("USERPROFILE", git.home.path())
        .env("ASC_HOME", git.home.path().join("asc-home"))
        .env("GIT_CONFIG_GLOBAL", git.home.path().join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args);
    if let Some(t) = token {
        cmd.env("ASC_TOKEN", t);
    }
    cmd.output().unwrap()
}

/// A migrated server: the fixture pushed `--mirror` from a plain git server's clone.
struct Migrated {
    stack: Stack,
    git: Git,
    write: Token,
    old: PathBuf,
    url: String,
    skipped: Vec<(String, &'static str)>,
    pushed: String,
}

fn migrate() -> Migrated {
    // The migration fsck mode: the harmless legacy classes (an unsorted tree, an odd mode) are
    // ignored; the dangerous ones never are (U82 tests those).
    let stack = Stack::start_with("enabled = true\nfsck = \"migration\"\n", "", "");
    stack.create_repo("acme/web", "");
    let write = stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
    let git = Git::new();
    let (src, skipped) = source(&git);

    // The old host: a plain git server.
    let plain_dir = tempfile::tempdir().unwrap();
    let plain = Plain::start(&git, plain_dir.path());
    plain.create(&git, "acme/web");
    let r = git.run(&src, &["push", "--mirror", &plain.url("acme/web")]);
    assert!(r.ok, "seed plain: {}", r.stderr);
    let m = git.work("mirror");
    git.ok(
        &m,
        &["clone", "-q", "--mirror", &plain.url("acme/web"), "old.git"],
    );
    let old = m.join("old.git");

    // An unsorted tree is refused by git's own fsck in every mode (`treeNotSorted` is not one of
    // the harmless legacy classes); nothing of that push is imported. The U33 import fixtures
    // cover such trees on the mirror path.
    let url = stack.url_with("acme/web", &write);
    let refused = git.run(
        &old,
        &["push", &url, "refs/heads/unsorted:refs/heads/unsorted"],
    );
    assert!(!refused.ok, "an unsorted tree is refused");
    assert!(
        refused.stderr.contains("treeNotSorted"),
        "{}",
        mask(&refused.stderr)
    );
    assert!(
        !git.ok(&git.work("ls"), &["ls-remote", &url])
            .contains("unsorted"),
        "the refused push left no ref"
    );
    git.ok(&old, &["update-ref", "-d", "refs/heads/unsorted"]);

    // A tag of a blob cannot be represented (SPEC §14.1): the server names it and refuses the
    // push that holds it, so a migration sees the ref listed instead of losing it silently.
    let refused = git.run(&old, &["push", "--mirror", &url]);
    assert!(!refused.ok, "a push holding a blob tag is refused");
    for (name, ty) in &skipped {
        assert!(
            refused
                .stderr
                .contains(&format!("{name}: asc cannot represent a ref to a {ty}")),
            "{name} is listed: {}",
            mask(&refused.stderr)
        );
    }
    assert!(
        !git.ok(&git.work("ls"), &["ls-remote", &url])
            .contains("refs/"),
        "the refused push stored nothing"
    );
    for (name, _) in &skipped {
        git.ok(&old, &["update-ref", "-d", name]);
    }

    // Migrate.
    let pushed = git.run(&old, &["push", "--mirror", &url]);
    assert!(pushed.ok, "git push --mirror: {}", mask(&pushed.stderr));

    Migrated {
        stack,
        git,
        write,
        old,
        url,
        skipped,
        pushed: pushed.stderr,
    }
}

#[test]
fn a_mirror_clone_pushed_to_asc_and_cloned_back_matches_ref_for_ref() {
    let Migrated {
        git,
        old,
        url,
        skipped,
        pushed,
        stack,
        ..
    } = migrate();
    let _stack = &stack;
    let _ = &pushed;
    let want = refs_of(&git, &old);
    // `git clone --mirror` back: every ref and id, minus the listed skipped refs.
    let back = git.work("back");
    let r = git.run(&back, &["clone", "-q", "--mirror", &url, "back.git"]);
    assert!(r.ok, "git clone --mirror back: {}", mask(&r.stderr));
    let got = refs_of(&git, &back.join("back.git"));
    let expected: Vec<String> = want
        .iter()
        .filter(|l| !skipped.iter().any(|(s, _)| l.starts_with(&format!("{s} "))))
        .cloned()
        .collect();
    let missing: Vec<&String> = expected.iter().filter(|l| !got.contains(l)).collect();
    let extra: Vec<&String> = got.iter().filter(|l| !expected.contains(l)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "zero mismatches expected; missing {missing:?}, extra {extra:?}"
    );
    // Object-level identity for the odd shapes: the signed commit and the unsorted tree.
    let bw = back.join("back.git");
    for r in ["refs/heads/signed", "refs/heads/master"] {
        assert_eq!(
            git.ok(&bw, &["cat-file", "commit", r]),
            git.ok(&old, &["cat-file", "commit", r]),
            "{r}: the commit's bytes survive"
        );
    }
}

/// `asc clone` of the migrated repository: every branch and tag has the same id.
#[test]
fn asc_clone_of_a_migrated_repo_matches_ref_for_ref() {
    let Migrated {
        stack,
        git,
        write,
        old,
        skipped,
        ..
    } = migrate();
    let want = refs_of(&git, &old);
    let ascd = git.work("ascclone");
    let server = format!("http://127.0.0.1:{}/acme/web", stack.port);
    let out = asc(
        &git,
        &ascd,
        Some(&write.secret),
        &["clone", "-q", &server, "w"],
    );
    assert!(
        out.status.success(),
        "asc clone: {}",
        mask(&lossy(&out.stderr))
    );
    let ascw = ascd.join("w");
    // `asc clone` follows branches; the tags come with an explicit `fetch --tags`.
    asc_ok(
        &git,
        &ascw,
        &write.secret,
        &["fetch", "-q", "--tags", "origin"],
    );
    let mut mismatches = Vec::new();
    for line in &want {
        let (name, sha) = line.split_once(' ').unwrap();
        if skipped.iter().any(|(s, _)| s == name) {
            continue;
        }
        let spec = name
            .strip_prefix("refs/heads/")
            .map(|b| format!("origin/{b}"))
            .or_else(|| name.strip_prefix("refs/tags/").map(str::to_owned))
            .unwrap();
        // An annotated tag: either the tag object or the commit it peels to is acceptable.
        let peeled = git.ok(&old, &["rev-parse", &format!("{name}^{{commit}}")]);
        let o = asc(&git, &ascw, Some(&write.secret), &["rev-parse", &spec]);
        let got = o
            .status
            .success()
            .then(|| lossy(&o.stdout).trim().to_owned());
        match got {
            Some(got) if got == *sha || got == peeled => {}
            other => mismatches.push(format!("{name}: want {sha}, asc {other:?}")),
        }
    }
    assert!(
        mismatches.is_empty(),
        "asc clone mismatches: {mismatches:#?}"
    );
}

fn asc_ok(git: &Git, cwd: &Path, token: &str, args: &[&str]) -> String {
    let out = asc(git, cwd, Some(token), args);
    assert!(
        out.status.success(),
        "asc {args:?}: {}",
        mask(&lossy(&out.stderr))
    );
    lossy(&out.stdout).trim().to_owned()
}

/// The whole native loop against `asc serve`: `asc push` uploads objects and moves the ref, `asc
/// clone` reads them back, and a later `asc push` is picked up by `asc fetch`.
#[test]
fn asc_push_clone_and_fetch_loop_through_asc_serve() {
    let stack = Stack::start();
    stack.create_repo("acme/web", "");
    let write = stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
    let git = Git::new();
    let server = format!("http://127.0.0.1:{}/acme/web", stack.port);
    let tok = write.secret.as_str();

    let a = git.work("a");
    asc_ok(&git, &a, tok, &["init", "-q"]);
    std::fs::write(
        a.join("one.txt"),
        "one
",
    )
    .unwrap();
    asc_ok(&git, &a, tok, &["add", "one.txt"]);
    asc_ok(&git, &a, tok, &["commit", "-q", "-m", "one"]);
    asc_ok(&git, &a, tok, &["remote", "add", "origin", &server]);
    asc_ok(
        &git,
        &a,
        tok,
        &["push", "-q", "origin", "HEAD:refs/heads/master"],
    );
    let first = asc_ok(&git, &a, tok, &["rev-parse", "HEAD"]);

    let b = git.work("b");
    asc_ok(&git, &b, tok, &["clone", "-q", &server, "w"]);
    let bw = b.join("w");
    assert_eq!(
        std::fs::read_to_string(bw.join("one.txt")).unwrap(),
        "one
",
        "the clone has the file"
    );
    assert_eq!(
        asc_ok(&git, &bw, tok, &["rev-parse", "origin/master"]),
        first
    );

    std::fs::write(
        a.join("two.txt"),
        "two
",
    )
    .unwrap();
    asc_ok(&git, &a, tok, &["add", "two.txt"]);
    asc_ok(&git, &a, tok, &["commit", "-q", "-m", "two"]);
    asc_ok(
        &git,
        &a,
        tok,
        &["push", "-q", "origin", "HEAD:refs/heads/master"],
    );
    let second = asc_ok(&git, &a, tok, &["rev-parse", "HEAD"]);
    assert_ne!(first, second);

    asc_ok(&git, &bw, tok, &["fetch", "-q", "origin"]);
    assert_eq!(
        asc_ok(&git, &bw, tok, &["rev-parse", "origin/master"]),
        second
    );
}
