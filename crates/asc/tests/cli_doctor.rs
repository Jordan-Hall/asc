//! U40 acceptance: `asc doctor` and `asc gc` through the built binary.

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{Fixture, Sandbox, text};

/// Real git in `dir`, hermetic.
fn git(dir: &Path, sb: &Sandbox, args: &[&str]) {
    let out = Command::new("git") // lint:allow-raw-git
        .current_dir(dir)
        .env("HOME", &sb.user)
        .env("USERPROFILE", &sb.user)
        .env("GIT_CONFIG_GLOBAL", sb.user.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
}

/// The directory that holds the first `git` on the test process's `PATH`.
fn git_dir() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&path)
        .find(|d| d.join(name).is_file())
        .expect("git on PATH")
}

fn shim_dir() -> PathBuf {
    let exe = PathBuf::from(support::asc_binary().expect("integration tests only"));
    exe.parent().unwrap().join("shim")
}

fn stdout(out: &Output) -> String {
    text(&out.stdout)
}

fn line<'a>(out: &'a str, prefix: &str) -> &'a str {
    out.lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starting {prefix:?} in:\n{out}"))
}

/// `asc doctor` outside a workspace with `PATH` set to `path`.
fn doctor_with_path(sb: &Sandbox, path: &OsString, args: &[&str]) -> Output {
    let cwd = sb.dir("elsewhere");
    sb.asc(&cwd)
        .env("PATH", path)
        .env("ASC_NO_DAEMON", "1")
        .arg("doctor")
        .args(args)
        .output()
        .expect("spawn asc")
}

/// A path -> size map of every file under `root`, for "nothing was deleted".
fn snapshot(root: &Path) -> BTreeMap<PathBuf, u64> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), m.len());
            }
        }
    }
    out
}

fn write_bytes(path: &Path, n: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![7u8; n]).unwrap();
}

#[test]
fn a_path_without_the_shim_is_reported() {
    let sb = Sandbox::new();
    let out = doctor_with_path(&sb, &git_dir().into_os_string(), &[]);
    let o = stdout(&out);
    let l = line(&o, "warn  git@turbo");
    assert!(l.contains("not the asc shim"), "{l}");
    assert!(
        o.contains("fix: put ") && o.contains("first on PATH"),
        "{o}"
    );
    line(&o, "ok    real-git");
    line(&o, "ok    daemon");
    line(&o, "ok    asc-home-space");
    assert!(out.status.code() == Some(0), "{o}\n{}", text(&out.stderr));
}

#[test]
fn a_path_with_the_shim_first_is_accepted() {
    let sb = Sandbox::new();
    let cwd = sb.dir("elsewhere");
    let install = sb.asc(&cwd).arg("install-shim").output().unwrap();
    assert!(install.status.success(), "{}", text(&install.stderr));
    let path = std::env::join_paths([shim_dir(), git_dir()]).unwrap();
    let out = doctor_with_path(&sb, &path, &[]);
    let o = stdout(&out);
    line(&o, "ok    shim-dir");
    let l = line(&o, "ok    git@turbo");
    assert!(l.contains("the asc shim"), "{l}");
    // Real git is still found behind the shim.
    line(&o, "ok    real-git");
}

#[test]
fn fix_installs_the_shim_dir() {
    // A private copy of the binary, so its `shim` dir is new.
    let sb = Sandbox::new();
    let bin = sb.dir("bin");
    let exe = bin.join(if cfg!(windows) { "asc.exe" } else { "asc" });
    fs::copy(support::asc_binary().unwrap(), &exe).unwrap();
    assert!(!bin.join("shim").exists());
    let cwd = sb.dir("elsewhere");
    let before = sb
        .command(&exe, &cwd)
        .env("PATH", git_dir())
        .env("ASC_NO_DAEMON", "1")
        .arg("doctor")
        .output()
        .unwrap();
    assert!(
        line(&stdout(&before), "warn  shim-dir").contains("does not exist"),
        "{}",
        stdout(&before)
    );
    let out = sb
        .command(&exe, &cwd)
        .env("PATH", git_dir())
        .env("ASC_NO_DAEMON", "1")
        .args(["doctor", "--fix"])
        .output()
        .unwrap();
    let o = stdout(&out);
    assert!(o.contains("fixed Shim"), "{o}");
    line(&o, "ok    shim-dir");
}

#[test]
fn reclaimable_report_sizes_a_repo_with_worktrees_and_deletes_nothing() {
    let sb = Sandbox::new();
    let repo = sb.dir("old");
    git(&repo, &sb, &["init", "-q", "-b", "master"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &sb, &["add", "-A"]);
    git(&repo, &sb, &["commit", "-q", "-m", "one"]);
    for name in ["wt1", "wt2"] {
        let wt = sb.tmp.path().join(name);
        git(
            &repo,
            &sb,
            &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()],
        );
        write_bytes(&wt.join("data.bin"), 1000);
    }
    let dot = repo.join(".git");
    write_bytes(&dot.join("modules/sub/objects/x"), 3000);
    write_bytes(&dot.join("lfs/objects/aa/bb/blob"), 4096);
    let pack = dot.join("objects/pack");
    fs::create_dir_all(&pack).unwrap();
    write_bytes(&pack.join("pack-cruft.pack"), 1000);
    write_bytes(&pack.join("pack-cruft.idx"), 200);
    write_bytes(&pack.join("pack-cruft.mtimes"), 50);
    write_bytes(&pack.join("pack-live.pack"), 700);
    write_bytes(&pack.join("pack-live.idx"), 100);

    let before = snapshot(sb.tmp.path());
    let out = sb.run(
        &sb.dir("elsewhere"),
        &["doctor", "--report-reclaimable", repo.to_str().unwrap()],
    );
    let o = stdout(&out);
    assert!(out.status.success(), "{o}\n{}", text(&out.stderr));
    assert!(line(&o, "  modules/:").contains("3000 bytes"), "{o}");
    assert!(line(&o, "  modules/:").contains("in 1 entries"), "{o}");
    assert!(line(&o, "  lfs/:").contains("4096 bytes"), "{o}");
    assert!(
        line(&o, "  cruft packs:").starts_with("  cruft packs: 1250 bytes"),
        "{o}"
    );
    assert!(line(&o, "  cruft packs:").ends_with("in 1 packs"), "{o}");
    assert!(line(&o, "  worktrees/:").contains("in 2 entries"), "{o}");
    let wts = line(&o, "  linked worktree checkouts:");
    assert!(wts.contains("in 2 of 2 worktrees"), "{wts}");
    // Each checkout holds `data.bin` (1000 bytes) plus its checked-out files and `.git` file.
    let bytes: u64 = wts
        .trim_start_matches("  linked worktree checkouts: ")
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(bytes >= 2000, "{wts}");
    let total: u64 = line(&o, "  .git:")
        .trim_start_matches("  .git: ")
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(total >= 3000 + 4096 + 1250 + 800, "{o}");
    assert!(o.trim_end().ends_with("nothing was deleted"), "{o}");
    assert_eq!(before, snapshot(sb.tmp.path()), "the report changed files");
}

#[test]
fn reclaimable_report_refuses_a_non_repository_and_exits_nonzero() {
    let sb = Sandbox::new();
    let plain = sb.dir("plain");
    let out = sb.run(
        &sb.dir("elsewhere"),
        &["doctor", "--report-reclaimable", plain.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("no .git directory"),
        "{}",
        text(&out.stderr)
    );
}

#[tokio::test]
async fn workspace_checks_report_ignore_shadows_and_include_if_drift() {
    let f = Fixture::new().await;
    fs::write(f.root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(f.root.join(".ascignore"), "*.tmp\n").unwrap();
    fs::create_dir_all(f.root.join("same")).unwrap();
    fs::write(f.root.join("same/.gitignore"), "x\n").unwrap();
    fs::write(f.root.join("same/.ascignore"), "x\n").unwrap();
    fs::write(f.root.join(".gitattributes"), "*.txt eol=crlf\n").unwrap();
    let root = f.root.display().to_string().replace('\\', "/");
    fs::write(
        f.sandbox.user.join(".gitconfig"),
        format!("[core]\n\tautocrlf = true\n[includeIf \"gitdir/i:{root}/\"]\n\tpath = other\n"),
    )
    .unwrap();
    let out = f
        .sandbox
        .asc(&f.root)
        .env("ASC_NO_DAEMON", "1")
        .arg("doctor")
        .output()
        .unwrap();
    let o = stdout(&out);
    let shadow = line(&o, "warn  ascignore");
    assert!(shadow.contains("1 directory"), "{shadow}");
    assert!(!shadow.contains("same"), "{shadow}");
    assert!(line(&o, "warn  gitattributes").contains("'eol'"), "{o}");
    line(&o, "warn  autocrlf");
    let drift = line(&o, "warn  include-if");
    assert!(drift.contains("applies in asc"), "{drift}");
    line(&o, "info  remote");
}

#[test]
fn gc_dry_run_changes_nothing_and_gc_evicts_the_cache_but_not_the_store() {
    let sb = Sandbox::new();
    fs::write(
        sb.user.join(".gitconfig"),
        "[asc \"cache\"]\n\tmaxsize = 1k\n",
    )
    .unwrap();
    let cwd = sb.dir("elsewhere");
    // Create the home layout, then fill the cache and the store.
    let first = sb.run(&cwd, &["gc", "--dry-run"]);
    assert!(first.status.success(), "{}", text(&first.stderr));
    let cache = sb.asc_home.join("cache");
    let store = sb.asc_home.join("store");
    for i in 0..8 {
        let hex = format!("{i:02x}").repeat(32);
        write_bytes(&cache.join(format!("f/{i:02x}/{hex}")), 1024);
    }
    write_bytes(&store.join("f/ff/keep"), 5000);
    let before = snapshot(&sb.asc_home);

    let dry = sb.run(&cwd, &["gc", "--dry-run"]);
    let o = stdout(&dry);
    assert!(dry.status.success(), "{o}");
    assert!(o.contains("cache: 8192 bytes"), "{o}");
    assert!(o.contains("nothing was deleted"), "{o}");
    assert_eq!(before, snapshot(&sb.asc_home));

    let real = sb.run(&cwd, &["gc"]);
    let o = stdout(&real);
    assert!(real.status.success(), "{o}\n{}", text(&real.stderr));
    assert!(o.contains("cache: evicted"), "{o}");
    let left: u64 = snapshot(&cache).values().sum();
    assert!(left <= 819, "cache left {left} bytes");
    assert!(
        store.join("f/ff/keep").is_file(),
        "the store is never evicted"
    );
}

#[test]
fn gc_refuses_git_flags_it_cannot_honour() {
    let sb = Sandbox::new();
    let cwd = sb.dir("elsewhere");
    for args in [&["gc", "--prune=now"][..], &["gc", "--aggressive"]] {
        let out = sb.run(&cwd, args);
        assert_eq!(
            out.status.code(),
            Some(129),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    let quiet = sb.run(&cwd, &["gc", "-q", "--auto"]);
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());
}

#[test]
fn gc_store_requires_a_url() {
    let sb = Sandbox::new();
    let out = sb.run(&sb.dir("elsewhere"), &["gc", "--store"]);
    assert_eq!(out.status.code(), Some(129));
}

#[test]
fn store_probe_passes_on_a_file_store() {
    let sb = Sandbox::new();
    let bucket = sb.dir("bucket");
    let url = format!(
        "file:///{}",
        bucket
            .display()
            .to_string()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let out = doctor_with_path(&sb, &git_dir().into_os_string(), &["--store", &url]);
    let o = stdout(&out);
    line(&o, "ok    store");
}
