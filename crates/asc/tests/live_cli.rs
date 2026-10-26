//! B31 and B30 acceptance (SPEC §21.3.8) through the built binary.
//!
//! `asc live pause|resume|status` run against a real `asc daemon run` on a private endpoint
//! (`ASC_DAEMON_ENDPOINT`), so the user's own daemon is never involved. With the `serve` feature
//! the same binary is run against an in-process asc server: `asc live run` (the pass the daemon's
//! production runner starts: fetch, restack, move, conflict pause, safety gates, no hooks) and the
//! daemon itself, which finds a server repository, runs the pass and records it.

#[cfg(feature = "serve")]
mod server_fixture;
mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::Duration;

use support::{Sandbox, text};

const ABSENT: &str = "the daemon is not running";

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Rig {
    sb: Sandbox,
    root: PathBuf,
    endpoint: String,
}

impl Rig {
    fn new() -> Self {
        let sb = Sandbox::new();
        let root = sb.dir("repo");
        let endpoint = if cfg!(windows) {
            format!(
                "asc-b31-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )
        } else {
            sb.dir("ipc").join("d.sock").to_string_lossy().into_owned()
        };
        let rig = Self { sb, root, endpoint };
        assert!(rig.asc(&["init", "-q"]).status.success());
        rig
    }

    fn cmd(&self, args: &[&str]) -> std::process::Command {
        let mut c = self.sb.asc(&self.root);
        c.env("ASC_DAEMON_ENDPOINT", &self.endpoint)
            .stdin(Stdio::null())
            .args(args);
        c
    }

    fn asc(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("spawn asc")
    }

    fn daemon(&self) -> Daemon {
        let child = self
            .cmd(&["daemon", "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the daemon");
        let d = Daemon(child);
        for _ in 0..200 {
            if self.asc(&["daemon", "status"]).status.success() {
                return d;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not come up");
    }

    fn status_json(&self) -> serde_json::Value {
        let out = self.asc(&["live", "status", "--json"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        serde_json::from_slice(&out.stdout).expect("json")
    }

    fn current(&self) -> serde_json::Value {
        self.status_json()["worktrees"][0].clone()
    }
}

fn state_path(rig: &Rig, id: &str) -> PathBuf {
    rig.sb.asc_home.join("live").join(format!("{id}.json"))
}

#[test]
fn verbs_fail_with_the_daemon_absent_message() {
    let rig = Rig::new();
    for args in [
        &["live", "pause"][..],
        &["live", "resume"],
        &["live", "status"],
        &["live", "status", "--json"],
    ] {
        let out = rig.asc(args);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out.stderr).contains(ABSENT), "{}", text(&out.stderr));
    }
    // ASC_NO_DAEMON gives the same answer, and nothing was written.
    let out = rig
        .cmd(&["live", "pause"])
        .env("ASC_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(!out.status.success() && text(&out.stderr).contains(ABSENT));
    assert!(!Path::new(&rig.sb.asc_home.join("live")).exists());
}

#[test]
fn pause_survives_a_daemon_restart_and_resume_clears_it() {
    let rig = Rig::new();
    let d = rig.daemon();
    let w = rig.current();
    assert_eq!(w["paused"], false);
    assert_eq!(w["mode"], "fetch");

    assert!(rig.asc(&["live", "pause"]).status.success());
    let w = rig.current();
    assert_eq!(
        (w["paused"].clone(), w["mode"].clone()),
        (true.into(), "paused".into())
    );
    assert_eq!(w["pauseReason"], "user");
    let text_out = text(&rig.asc(&["live", "status"]).stdout);
    assert!(text_out.contains("paused (user)"), "{text_out}");

    drop(d);
    let d = rig.daemon();
    assert_eq!(rig.current()["paused"], true);

    assert!(rig.asc(&["live", "resume"]).status.success());
    let w = rig.current();
    assert_eq!(w["paused"], false);
    assert!(w["pauseReason"].is_null());
    drop(d);
}

#[test]
fn the_conflict_notice_shows_in_status_and_sl_until_resume() {
    let rig = Rig::new();
    let _d = rig.daemon();
    let id = rig.current()["id"].as_str().unwrap().to_owned();
    let path = rig.current()["worktree"].as_str().unwrap().to_owned();
    let none = |o: &Output| !text(&o.stderr).contains("asc live resume");
    assert!(none(&rig.asc(&["status"])));

    // What the daemon records when a live run hits a conflict.
    std::fs::create_dir_all(state_path(&rig, &id).parent().unwrap()).unwrap();
    let repo = rig.status_json()["worktrees"][0]["worktree"].clone();
    assert_eq!(repo, path.as_str());
    let state = serde_json::json!({
        "version": 1, "worktree": path, "repo": 1, "paused": true,
        "reason": "conflict", "detail": "a.txt", "since": 1,
    });
    std::fs::write(state_path(&rig, &id), state.to_string()).unwrap();

    let status = rig.asc(&["status"]);
    assert!(
        text(&status.stderr).contains("asc live resume"),
        "{}",
        text(&status.stderr)
    );
    assert!(!text(&status.stdout).contains("asc live resume"));
    let sl = rig.asc(&["sl"]);
    assert!(
        text(&sl.stderr).contains("asc live resume"),
        "{}",
        text(&sl.stderr)
    );
    // Machine formats stay clean.
    assert!(none(&rig.asc(&["status", "--porcelain=v2"])));
    assert!(none(&rig.asc(&["sl", "--json"])));
    assert_eq!(rig.current()["pauseReason"], "conflict");

    assert!(rig.asc(&["live", "resume"]).status.success());
    assert!(none(&rig.asc(&["status"])));
    assert!(none(&rig.asc(&["sl"])));
}

#[test]
fn a_user_pause_shows_no_conflict_notice() {
    let rig = Rig::new();
    let _d = rig.daemon();
    assert!(rig.asc(&["live", "pause"]).status.success());
    assert!(!text(&rig.asc(&["status"]).stderr).contains("asc live resume"));
}

// ------------------------------------------------------------------------------ B30: the pass

#[cfg(feature = "serve")]
mod live_run {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Output, Stdio};
    use std::time::{Duration, Instant};

    use super::server_fixture::{Server, WRITE};
    use super::support::{Sandbox, text};

    const IDENT: [(&str, &str); 6] = [
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_AUTHOR_DATE", "1700000000 +0000"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_DATE", "1700000000 +0000"),
    ];

    /// A server, a client `a` that pushes trunk and a client `b` whose worktree the pass runs in.
    struct World {
        _server: Server,
        asb: Sandbox,
        a: PathBuf,
        bsb: Sandbox,
        b: PathBuf,
    }

    fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
        sb.asc(cwd)
            .envs(IDENT)
            .env("ASC_TOKEN", WRITE)
            .env("ASC_NO_DAEMON", "1")
            .stdin(Stdio::null())
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

    fn write_commit(sb: &Sandbox, repo: &Path, name: &str, body: &str) {
        std::fs::write(repo.join(name), body).unwrap();
        ok(sb, repo, &["add", name]);
        ok(sb, repo, &["commit", "-q", "-m", name]);
    }

    fn slash(p: &Path) -> String {
        p.display().to_string().replace('\\', "/")
    }

    impl World {
        fn new() -> Self {
            let server = Server::start(&["acme/app"]);
            let url = server.url("acme/app");
            let asb = Sandbox::new();
            let a = asb.dir("a");
            ok(&asb, &a, &["init", "-q"]);
            ok(&asb, &a, &["remote", "add", "origin", &url]);
            write_commit(&asb, &a, "one.txt", "one\n");
            ok(&asb, &a, &["push", "origin", "master"]);
            let bsb = Sandbox::new();
            let root = bsb.tmp.path().to_owned();
            ok(&bsb, &root, &["clone", "-q", &url, "b"]);
            let b = root.join("b");
            Self {
                _server: server,
                asb,
                a,
                bsb,
                b,
            }
        }

        /// Trunk moves: client `a` commits a file and pushes `master`.
        fn trunk_moves(&self, name: &str, body: &str) {
            write_commit(&self.asb, &self.a, name, body);
            ok(&self.asb, &self.a, &["push", "origin", "master"]);
        }

        fn b(&self, args: &[&str]) -> String {
            ok(&self.bsb, &self.b, args)
        }

        fn rev(&self, rev: &str) -> String {
            self.b(&["rev-parse", rev]).trim().to_owned()
        }

        /// A draft commit on a new branch `feature`, with HEAD back on `master`.
        fn feature_draft(&self, name: &str, body: &str) {
            self.b(&["switch", "-q", "-c", "feature"]);
            write_commit(&self.bsb, &self.b, name, body);
            self.b(&["switch", "-q", "master"]);
        }

        fn set(&self, key: &str, value: &str) {
            self.b(&["config", key, value]);
        }

        /// `asc live run` the way the daemon starts it: no daemon, no terminal, and the empty
        /// hooks directory of the live pass.
        fn live_with(&self, hooks: Option<&Path>) -> Output {
            let mut cmd = self.bsb.asc(&self.b);
            cmd.envs(IDENT)
                .env("ASC_TOKEN", WRITE)
                .env("ASC_NO_DAEMON", "1")
                .stdin(Stdio::null())
                .args(["live", "run"]);
            if let Some(dir) = hooks {
                cmd.env("GIT_CONFIG_COUNT", "1")
                    .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                    .env("GIT_CONFIG_VALUE_0", dir);
            }
            cmd.output().expect("spawn asc")
        }

        fn hookless(&self) -> PathBuf {
            let dir = self.bsb.asc_home.join("live").join("no-hooks");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// The one-line result of a pass; the pass must succeed.
        fn live(&self) -> String {
            let out = self.live_with(Some(&self.hookless()));
            assert!(
                out.status.success(),
                "asc live run failed: {}",
                text(&out.stderr)
            );
            text(&out.stdout).trim().to_owned()
        }

        fn state_files(&self) -> Vec<PathBuf> {
            std::fs::read_dir(self.bsb.asc_home.join("live"))
                .map(|d| {
                    d.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|e| e == "json"))
                        .collect()
                })
                .unwrap_or_default()
        }

        fn state(&self) -> serde_json::Value {
            let files = self.state_files();
            assert_eq!(files.len(), 1, "one live state file: {files:?}");
            serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap()
        }

        fn tree(&self, rev: &str) -> Vec<String> {
            self.b(&["ls-tree", "-r", "--name-only", rev])
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[test]
    fn a_pass_needs_the_daemons_empty_hooks_path() {
        let w = World::new();
        let out = w.live_with(None);
        assert!(!out.status.success());
        assert!(
            text(&out.stderr).contains("started by the daemon"),
            "{}",
            text(&out.stderr)
        );
        // A hooks path that is not the daemon's empty directory is refused too.
        let other = w.bsb.dir("other-hooks");
        let out = w.live_with(Some(&other));
        assert!(!out.status.success());
        assert!(w.state_files().is_empty(), "a refused pass records nothing");
    }

    #[test]
    fn a_pass_fetches_the_new_trunk_and_records_its_run() {
        let w = World::new();
        let before = w.rev("origin/master");
        w.trunk_moves("two.txt", "two\n");
        assert_eq!(w.rev("origin/master"), before, "nothing fetched yet");

        let line = w.live();
        assert!(line.ends_with(": fetched"), "{line}");
        assert_ne!(w.rev("origin/master"), before, "the new trunk is here");
        assert_eq!(w.rev("master"), before, "fetch-only moves no branch");
        assert!(!w.b.join("two.txt").exists(), "and no file");

        let state = w.state();
        assert_eq!(state["last_run"]["outcome"], "fetched");
        assert_eq!(state["paused"], false);
    }

    #[test]
    fn a_pass_runs_no_hook_of_the_users() {
        let w = World::new();
        let marker = w.bsb.tmp.path().join("hook-ran");
        let hooks = w.bsb.dir("hooks");
        let script = hooks.join("reference-transaction");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$1\" >> \"{}\"\n", slash(&marker)),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        w.set("core.hooksPath", &slash(&hooks));

        // The control: an ordinary fetch runs the hook.
        w.trunk_moves("two.txt", "two\n");
        w.b(&["fetch", "-q", "origin"]);
        assert!(marker.exists(), "the hook runs for an ordinary fetch");
        std::fs::remove_file(&marker).unwrap();

        w.trunk_moves("three.txt", "three\n");
        let line = w.live();
        assert!(line.ends_with(": fetched"), "{line}");
        assert!(!marker.exists(), "no hook ran on a live update");
    }

    #[test]
    fn restack_moves_the_branches_that_follow_a_draft_but_not_the_checkout() {
        let w = World::new();
        w.feature_draft("mine.txt", "mine\n");
        w.set("asc.live.restack", "true");
        let old_feature = w.rev("feature");
        let old_master = w.rev("master");
        w.trunk_moves("trunk.txt", "trunk\n");

        let line = w.live();
        assert!(line.ends_with(": restacked: 1 change restacked"), "{line}");
        assert_ne!(w.rev("feature"), old_feature, "the draft was rewritten");
        assert_eq!(
            w.rev("feature~1"),
            w.rev("origin/master"),
            "onto the new trunk"
        );
        let files = w.tree("feature");
        assert!(files.contains(&"mine.txt".to_owned()) && files.contains(&"trunk.txt".to_owned()));
        // The checkout stays where it was, with the files it had.
        assert_eq!(w.rev("HEAD"), old_master);
        assert!(!w.b.join("trunk.txt").exists());
        assert_eq!(w.state()["last_run"]["outcome"], "restacked");

        // The next pass finds nothing to do.
        assert!(w.live().ends_with(": fetched"));
    }

    #[test]
    fn update_moves_the_checkout_to_the_new_trunk() {
        let w = World::new();
        w.set("asc.live.update", "true");
        let old = w.rev("HEAD");
        w.trunk_moves("trunk.txt", "trunk\n");

        let line = w.live();
        assert!(line.ends_with(": updated: 0 changes restacked"), "{line}");
        let tip = w.rev("origin/master");
        assert_ne!(tip, old);
        assert_eq!(w.rev("HEAD"), tip);
        assert_eq!(w.rev("master"), tip, "the trunk branch followed");
        assert_eq!(
            std::fs::read_to_string(w.b.join("trunk.txt")).unwrap(),
            "trunk\n"
        );
        assert_eq!(w.b(&["status", "--porcelain"]).trim(), "", "a clean tree");
        assert_eq!(w.state()["last_run"]["outcome"], "updated");
    }

    #[test]
    fn update_moves_the_checked_out_draft_onto_the_new_trunk() {
        let w = World::new();
        w.b(&["switch", "-q", "-c", "feature"]);
        write_commit(&w.bsb, &w.b, "mine.txt", "mine\n");
        w.set("asc.live.update", "true");
        let old = w.rev("HEAD");
        w.trunk_moves("trunk.txt", "trunk\n");

        let line = w.live();
        assert!(line.ends_with(": updated: 1 change restacked"), "{line}");
        assert_ne!(w.rev("HEAD"), old);
        assert_eq!(w.rev("HEAD~1"), w.rev("origin/master"));
        assert_eq!(
            w.rev("feature"),
            w.rev("HEAD"),
            "the branch moved with HEAD"
        );
        assert_eq!(
            std::fs::read_to_string(w.b.join("trunk.txt")).unwrap(),
            "trunk\n"
        );
        assert_eq!(
            std::fs::read_to_string(w.b.join("mine.txt")).unwrap(),
            "mine\n"
        );
        assert_eq!(w.b(&["status", "--porcelain"]).trim(), "");

        // One op-log entry of kind live; `asc undo` puts the old commit back.
        w.b(&["undo"]);
        assert_eq!(w.rev("HEAD"), old, "undo reverts the live update");
    }

    #[test]
    fn a_dirty_tree_is_never_moved() {
        let w = World::new();
        w.b(&["switch", "-q", "-c", "feature"]);
        write_commit(&w.bsb, &w.b, "mine.txt", "mine\n");
        w.set("asc.live.update", "true");
        let old = w.rev("HEAD");
        std::fs::write(w.b.join("mine.txt"), "edited\n").unwrap();
        w.trunk_moves("trunk.txt", "trunk\n");

        let line = w.live();
        assert!(
            line.ends_with(
                ": restacked: 1 change restacked; working copy not moved: local changes"
            ),
            "{line}"
        );
        assert_eq!(w.rev("HEAD"), old);
        assert_eq!(w.rev("feature"), old, "the branch under HEAD stays");
        assert_eq!(
            std::fs::read_to_string(w.b.join("mine.txt")).unwrap(),
            "edited\n"
        );
        assert!(!w.b.join("trunk.txt").exists());
    }

    #[test]
    fn an_untracked_file_in_the_way_is_never_overwritten() {
        let w = World::new();
        w.set("asc.live.update", "true");
        let old = w.rev("HEAD");
        w.trunk_moves("trunk.txt", "trunk\n");
        std::fs::write(w.b.join("trunk.txt"), "mine, untracked\n").unwrap();

        let line = w.live();
        assert!(
            line.ends_with(": fetched") || line.contains("working copy not moved"),
            "{line}"
        );
        assert_eq!(w.rev("HEAD"), old);
        assert_eq!(
            std::fs::read_to_string(w.b.join("trunk.txt")).unwrap(),
            "mine, untracked\n"
        );
    }

    #[test]
    fn a_conflict_restacks_nothing_and_pauses_until_resume() {
        let w = World::new();
        w.b(&["switch", "-q", "-c", "feature"]);
        write_commit(&w.bsb, &w.b, "one.txt", "mine\n");
        w.b(&["switch", "-q", "master"]);
        w.set("asc.live.restack", "true");
        let old = w.rev("feature");
        w.trunk_moves("one.txt", "trunk\n");

        let line = w.live();
        assert!(line.contains(": notice: restack conflicts at "), "{line}");
        assert!(line.ends_with("live updates paused"), "{line}");
        assert_eq!(w.rev("feature"), old, "nothing was restacked");

        let state = w.state();
        assert_eq!(state["paused"], true);
        assert_eq!(state["reason"], "conflict");
        let status = asc(&w.bsb, &w.b, &["status"]);
        assert!(
            text(&status.stderr).contains("asc live resume"),
            "{}",
            text(&status.stderr)
        );

        // While paused a pass runs nothing, not even the fetch.
        w.trunk_moves("later.txt", "later\n");
        let before = w.rev("origin/master");
        assert!(w.live().ends_with(": paused"));
        assert_eq!(w.rev("origin/master"), before);
    }

    #[test]
    fn a_signing_policy_stops_a_restack_with_a_notice() {
        let w = World::new();
        w.feature_draft("mine.txt", "mine\n");
        w.set("asc.live.restack", "true");
        w.set("commit.gpgsign", "true");
        let old = w.rev("feature");
        w.trunk_moves("trunk.txt", "trunk\n");

        let line = w.live();
        assert!(line.contains(": notice: signing: commit.gpgSign"), "{line}");
        assert_eq!(w.rev("feature"), old);
        assert_eq!(w.state()["paused"], false, "only a conflict pauses");
    }

    #[test]
    fn with_every_live_key_off_a_pass_does_nothing() {
        let w = World::new();
        w.set("asc.live.fetch", "false");
        let before = w.rev("origin/master");
        w.trunk_moves("two.txt", "two\n");
        assert!(w.live().ends_with(": off"));
        assert_eq!(w.rev("origin/master"), before);
    }

    #[test]
    fn a_repository_that_is_not_a_asc_server_is_skipped() {
        let sb = Sandbox::new();
        let root = sb.dir("plain");
        ok(&sb, &root, &["init", "-q"]);
        let hooks = sb.asc_home.join("live").join("no-hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let out = sb
            .asc(&root)
            .envs(IDENT)
            .env("ASC_NO_DAEMON", "1")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", &hooks)
            .stdin(Stdio::null())
            .args(["live", "run"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        assert!(
            text(&out.stdout).contains(": skipped: not an asc server repository"),
            "{}",
            text(&out.stdout)
        );
    }

    // -------------------------------------------------------------------------- the daemon

    struct Daemon(Child);

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn endpoint(sb: &Sandbox) -> String {
        if cfg!(windows) {
            format!(
                "asc-b30-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )
        } else {
            sb.dir("ipc").join("d.sock").to_string_lossy().into_owned()
        }
    }

    #[test]
    fn the_daemon_runs_the_pass_for_a_server_repository_without_any_command() {
        let w = World::new();
        let before = w.rev("origin/master");
        w.trunk_moves("two.txt", "two\n");
        let ep = endpoint(&w.bsb);

        // The daemon allowed to run (no ASC_NO_DAEMON), on its own endpoint, with the token.
        let mut cmd = w.bsb.asc(&w.b);
        cmd.envs(IDENT)
            .env("ASC_TOKEN", WRITE)
            .env("ASC_DAEMON_ENDPOINT", &ep)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["daemon", "run"]);
        let _daemon = Daemon(cmd.spawn().expect("spawn the daemon"));

        // The debounced first window runs `asc live run` in the worktree: trunk arrives by itself.
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut moved = false;
        while Instant::now() < deadline {
            let out = asc(&w.bsb, &w.b, &["rev-parse", "origin/master"]);
            if out.status.success() && text(&out.stdout).trim() != before {
                moved = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(moved, "the daemon fetched the new trunk");

        // And it recorded the run, which `asc live status` shows.
        let mut status = serde_json::Value::Null;
        while Instant::now() < deadline {
            let out = w
                .bsb
                .asc(&w.b)
                .env("ASC_DAEMON_ENDPOINT", &ep)
                .stdin(Stdio::null())
                .args(["live", "status", "--json"])
                .output()
                .unwrap();
            if out.status.success() {
                status = serde_json::from_slice(&out.stdout).unwrap();
                if status["worktrees"][0]["lastRun"]["outcome"] == "fetched" {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(
            status["worktrees"][0]["lastRun"]["outcome"], "fetched",
            "{status}"
        );
    }
}
