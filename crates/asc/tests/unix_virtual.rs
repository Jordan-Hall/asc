//! Virtual workspaces on Linux end to end through the built binary (SPEC §10.3b): a real daemon
//! serves the clone over FUSE, and every command goes through the mount.
//!
//! The tests need `/dev/fuse` that the user can open and `fusermount3`, and they start a daemon:
//! `cargo test -p asc --test unix_virtual -- --ignored --test-threads=1`. CI runs them in the
//! `fuse` job.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
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

/// A sandbox whose virtual workspaces use FUSE, and whose daemon is stopped when the test ends,
/// pass or fail: stopping it unmounts every workspace it serves.
struct Fuse(Sandbox);

impl Fuse {
    fn new() -> Self {
        let sb = Sandbox::new();
        let home = sb.tmp.path().to_path_buf();
        ok(
            &sb,
            &home,
            &["config", "--global", "asc.vfs.backend", "fuse"],
        );
        Self(sb)
    }
}

impl std::ops::Deref for Fuse {
    type Target = Sandbox;
    fn deref(&self) -> &Sandbox {
        &self.0
    }
}

impl Drop for Fuse {
    fn drop(&mut self) {
        let _ = self
            .0
            .asc(self.0.tmp.path())
            .args(["daemon", "stop"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
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

/// A bucket holding `master` (a.txt, src/b.txt) and `side` (one more file); returns its URL.
fn seed(sb: &Sandbox) -> String {
    let src = sb.dir("src-repo");
    ok(sb, &src, &["init", "-q"]);
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(src.join("a.txt"), "a\n").unwrap();
    std::fs::write(src.join("src").join("b.txt"), "b\n").unwrap();
    ok(sb, &src, &["add", "."]);
    ok(sb, &src, &["commit", "-q", "-m", "base"]);
    ok(sb, &src, &["switch", "-q", "-c", "side"]);
    std::fs::write(src.join("c.txt"), "c\n").unwrap();
    ok(sb, &src, &["add", "c.txt"]);
    ok(sb, &src, &["commit", "-q", "-m", "side"]);
    ok(sb, &src, &["switch", "-q", "master"]);
    let url = url::Url::from_directory_path(sb.dir("bucket"))
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    ok(sb, &src, &["remote", "add", "origin", &url]);
    ok(sb, &src, &["push", "-q", "origin", "master", "side"]);
    url
}

fn virtual_clone(sb: &Sandbox, url: &str) -> PathBuf {
    let parent = sb.dir("work");
    ok(sb, &parent, &["clone", "-q", "--mode", "virtual", url, "v"]);
    parent.join("v")
}

/// Whether `dir` is a FUSE mount point (`/proc/self/mountinfo`).
fn is_fuse_mount(dir: &Path) -> bool {
    let want = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    info.lines().any(|l| {
        let mut parts = l.split(" - ");
        let left = parts.next().unwrap_or_default();
        let right = parts.next().unwrap_or_default();
        left.split(' ').nth(4).is_some_and(|m| Path::new(m) == want) && right.starts_with("fuse")
    })
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn clone_reads_edits_commits_and_switches() {
    let sb = Fuse::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    assert!(is_fuse_mount(&ws), "the clone is not a FUSE mount");
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");
    assert_eq!(read(&ws.join("src").join("b.txt")), "b\n");

    std::fs::write(ws.join("a.txt"), "a2\n").unwrap();
    std::fs::write(ws.join("new.txt"), "n\n").unwrap();
    std::fs::rename(ws.join("src").join("b.txt"), ws.join("src").join("b2.txt")).unwrap();
    let status = ok(&sb, &ws, &["status", "--porcelain"]);
    for want in [" M a.txt", "?? new.txt", " D src/b.txt", "?? src/b2.txt"] {
        assert!(status.contains(want), "{want:?} missing from {status:?}");
    }
    ok(&sb, &ws, &["add", "-A"]);
    ok(&sb, &ws, &["commit", "-q", "-m", "edit"]);
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");

    ok(&sb, &ws, &["switch", "-q", "side"]);
    assert_eq!(read(&ws.join("c.txt")), "c\n");
    assert_eq!(read(&ws.join("a.txt")), "a\n");
    assert!(!ws.join("new.txt").exists());
    assert!(ws.join("src").join("b.txt").is_file());
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");
    ok(&sb, &ws, &["switch", "-q", "master"]);
    assert_eq!(read(&ws.join("a.txt")), "a2\n");
    assert!(!ws.join("c.txt").exists());
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn a_switch_over_local_edits_is_refused_and_keeps_them() {
    let sb = Fuse::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    // `c.txt` exists only on `side`: an untracked file of that name would be overwritten.
    std::fs::write(ws.join("c.txt"), "mine\n").unwrap();
    let out = asc(&sb, &ws, &["switch", "-q", "side"]);
    assert!(!out.status.success(), "the switch went ahead");
    assert!(text(&out.stderr).contains("c.txt"), "{}", text(&out.stderr));
    assert_eq!(read(&ws.join("c.txt")), "mine\n");
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "?? c.txt\n");
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn reset_hard_restores_edits_and_deletions() {
    let sb = Fuse::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    std::fs::write(ws.join("a.txt"), "x\n").unwrap();
    std::fs::remove_file(ws.join("src").join("b.txt")).unwrap();
    ok(&sb, &ws, &["reset", "-q", "--hard"]);
    assert_eq!(read(&ws.join("a.txt")), "a\n");
    assert_eq!(read(&ws.join("src").join("b.txt")), "b\n");
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn a_watched_mount_gets_the_watcher_note_on_switch() {
    use rustix::fs::inotify::{CreateFlags, WatchFlags, add_watch, init};
    let sb = Fuse::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    let quiet = asc(&sb, &ws, &["switch", "side"]);
    assert!(quiet.status.success(), "{}", text(&quiet.stderr));
    assert!(
        !text(&quiet.stderr).contains("file watchers"),
        "{}",
        text(&quiet.stderr)
    );
    let fd = init(CreateFlags::CLOEXEC).unwrap();
    add_watch(&fd, &ws, WatchFlags::MODIFY | WatchFlags::CREATE).unwrap();
    let out = asc(&sb, &ws, &["switch", "master"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(
            "note: file watchers do not see this switch; restart watch processes or use polling"
        ),
        "{}",
        text(&out.stderr)
    );
    drop(fd);
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn convert_to_plain_keeps_edits_and_deletions() {
    let sb = Fuse::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    std::fs::write(ws.join("a.txt"), "edited\n").unwrap();
    std::fs::remove_file(ws.join("src").join("b.txt")).unwrap();
    ok(&sb, &ws, &["worktree", "convert", "--plain"]);
    assert!(!is_fuse_mount(&ws), "the converted root is still mounted");
    assert_eq!(read(&ws.join("a.txt")), "edited\n");
    assert!(!ws.join("src").join("b.txt").exists());
    assert_eq!(
        ok(&sb, &ws, &["status", "--porcelain"]),
        " M a.txt\n D src/b.txt\n"
    );
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn a_moved_worktree_keeps_its_edits_and_is_served_at_the_new_path() {
    let sb = Fuse::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    let wt = sb.dir("wts").join("w1");
    ok(
        &sb,
        &ws,
        &["worktree", "add", "-q", "-b", "w1", wt.to_str().unwrap()],
    );
    assert!(is_fuse_mount(&wt), "the worktree is not a FUSE mount");
    std::fs::write(wt.join("a.txt"), "moved\n").unwrap();
    std::fs::write(wt.join("new.txt"), "n\n").unwrap();
    std::fs::remove_file(wt.join("src").join("b.txt")).unwrap();
    let moved = sb.dir("wts").join("w2");
    ok(
        &sb,
        &ws,
        &[
            "worktree",
            "move",
            wt.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    assert!(!wt.exists(), "the old path is still there");
    assert!(is_fuse_mount(&moved), "the moved worktree is not mounted");
    assert_eq!(read(&moved.join("a.txt")), "moved\n");
    assert!(!moved.join("src").join("b.txt").exists());
    assert_eq!(
        ok(&sb, &moved, &["status", "--porcelain"]),
        " M a.txt\n D src/b.txt\n?? new.txt\n"
    );
    ok(
        &sb,
        &ws,
        &["worktree", "remove", "--force", moved.to_str().unwrap()],
    );
    assert!(!moved.exists());
}

/// `asc daemon stop` under the job's `ASC_HOME` when dropped, so the job's daemon and its mount
/// go away pass or fail.
struct JobDaemon<'a> {
    sb: &'a Sandbox,
    home: PathBuf,
}

impl Drop for JobDaemon<'_> {
    fn drop(&mut self) {
        let _ = self
            .sb
            .asc(self.sb.tmp.path())
            .env("ASC_HOME", &self.home)
            .args(["daemon", "stop"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// The super options of the mount at `dir` (`/proc/self/mountinfo`, after ` - <type> <source> `).
fn super_options(dir: &Path) -> String {
    let want = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    info.lines()
        .rev()
        .filter(|l| l.split(' ').nth(4).is_some_and(|m| Path::new(m) == want))
        .filter_map(|l| l.split(" - ").nth(1))
        .find_map(|r| r.split(' ').nth(2))
        .unwrap_or_default()
        .to_owned()
}

#[test]
#[ignore = "needs /dev/fuse and fusermount3, and starts a daemon"]
fn ci_checkout_virtual_serves_the_job_through_a_mount_other_users_may_read() {
    let sb = Fuse::new();
    let url = url::Url::from_directory_path(sb.dir("ci-bucket"))
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    let src = sb.dir("ci-src");
    ok(
        &sb,
        &src,
        &["init", "-q", "--refs", "shared", "--store", &url],
    );
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(src.join("bun.lock"), "lock\n").unwrap();
    std::fs::write(src.join("src").join("app.ts"), "app\n").unwrap();
    ok(&sb, &src, &["add", "."]);
    ok(&sb, &src, &["commit", "-q", "-m", "base"]);
    ok(&sb, &src, &["push", "-q", "-u", "origin", "master"]);
    let head = ok(&sb, &src, &["rev-parse", "HEAD"]).trim().to_owned();

    let job = sb.dir("job");
    let runner_temp = job.join("tmp");
    let _daemon = JobDaemon {
        sb: &sb,
        home: runner_temp.join("asc-home"),
    };
    let out = sb
        .asc(&job)
        .envs(IDENT)
        .env("GITHUB_TOKEN", "task-token")
        .env("RUNNER_TEMP", &runner_temp)
        .args(["ci", "checkout", "--repo", "asc/orbit", "--mode", "virtual"])
        .args(["--mount-access", "all"])
        .arg(format!("--rev={head}"))
        .args(["--dir", "work", "--server", "https://asc.example"])
        .args(["--remote-url", &url, "--shim-dir"])
        .arg(job.join("shim"))
        .output()
        .expect("spawn asc");

    let root = rustix::process::geteuid().is_root();
    let conf = std::fs::read_to_string("/etc/fuse.conf").unwrap_or_default();
    if !root && !asc_ci::mounts::user_allow_other(&conf) {
        // A runner that is not root and may not mount for other users is refused up front.
        assert!(!out.status.success());
        assert!(
            text(&out.stderr).contains("needs `user_allow_other` in /etc/fuse.conf"),
            "{}",
            text(&out.stderr)
        );
        assert!(!job.join("work").join("bun.lock").exists());
        return;
    }
    assert!(out.status.success(), "{}", text(&out.stderr));
    let work = job.join("work");
    assert!(
        is_fuse_mount(&work),
        "the job's work tree is not a FUSE mount"
    );
    let opts = super_options(&work);
    assert!(
        opts.split(',').any(|o| o == "allow_other"),
        "the mount does not let other users in: {opts}"
    );
    assert_eq!(read(&work.join("src").join("app.ts")), "app\n");
    // The tier stack, with the mount access, persists for a daemon that mounts it again.
    let in_job = |args: &[&str]| {
        let out = sb
            .asc(&work)
            .env("ASC_HOME", runner_temp.join("asc-home"))
            .envs(IDENT)
            .args(args)
            .output()
            .expect("spawn asc");
        assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    assert_eq!(in_job(&["rev-parse", "HEAD"]).trim(), head);
    assert_eq!(in_job(&["config", "asc.vfs.mountAccess"]).trim(), "all");
    assert_eq!(in_job(&["status", "--porcelain"]), "");
}
