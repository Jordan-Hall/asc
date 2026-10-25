//! B31 acceptance (SPEC §21.3.8): `asc live pause|resume|status` through the built binary, with a
//! real `asc daemon run` on a private endpoint (`ASC_DAEMON_ENDPOINT`), so the user's own daemon is
//! never involved.

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
