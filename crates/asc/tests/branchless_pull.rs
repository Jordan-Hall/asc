//! B15 acceptance (SPEC §21.3.4, §20.1 D4): the cloud pull of `fetch` is a best-effort,
//! non-blocking ask to the daemon. A failed pull, a missing daemon or a cloud that is off never
//! changes `fetch`'s exit code, stdout or stderr, and no request marker is left when no daemon
//! answers. The ask's own policy (gate, bounds, marker safety) is tested in `branchless::pull`.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::PathBuf;
use std::process::Output;

use server_fixture::{Server, WRITE};
use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

struct Client {
    sb: Sandbox,
    root: PathBuf,
}

/// How a run reaches the daemon.
#[derive(Clone, Copy)]
enum Daemon {
    /// `ASC_NO_DAEMON=1`: the reference run.
    Off,
    /// A daemon is allowed but nothing listens at the endpoint.
    Unreachable,
}

impl Client {
    fn new(url: &str) -> Self {
        let sb = Sandbox::new();
        let root = sb.dir("repo");
        let c = Self { sb, root };
        c.run(&["init", "-q"], Daemon::Off, &[]);
        c.run(&["remote", "add", "origin", url], Daemon::Off, &[]);
        c
    }

    fn run(&self, args: &[&str], d: Daemon, config: &[&str]) -> Output {
        let mut cmd = self.sb.asc(&self.root);
        cmd.envs(IDENT).env("ASC_TOKEN", WRITE);
        match d {
            Daemon::Off => {
                cmd.env("ASC_NO_DAEMON", "1");
            }
            Daemon::Unreachable => {
                cmd.env(
                    "ASC_DAEMON_ENDPOINT",
                    if cfg!(windows) {
                        format!(r"\\.\pipe\asc-b15-absent-{}", std::process::id())
                    } else {
                        self.sb
                            .tmp
                            .path()
                            .join("absent.sock")
                            .to_string_lossy()
                            .into_owned()
                    },
                );
            }
        }
        for c in config {
            cmd.args(["-c", c]);
        }
        cmd.stdin(std::process::Stdio::null())
            .args(args)
            .output()
            .expect("spawn asc")
    }

    fn marker(&self) -> PathBuf {
        self.sb.asc_home.join("run").join("cloud-sync-request")
    }
}

fn same(a: &Output, b: &Output, why: &str) {
    assert_eq!(a.status.code(), b.status.code(), "exit code: {why}");
    assert_eq!(text(&a.stdout), text(&b.stdout), "stdout: {why}");
    assert_eq!(text(&a.stderr), text(&b.stderr), "stderr: {why}");
}

#[test]
fn a_failed_cloud_pull_leaves_fetch_identical_to_no_daemon() {
    let server = Server::start(&["acme/app"]);
    let c = Client::new(&server.url("acme/app"));
    // The first fetch settles the repo; later runs see the same state.
    let _ = c.run(&["fetch"], Daemon::Off, &[]);

    let reference = c.run(&["fetch"], Daemon::Off, &[]);
    let unreachable = c.run(&["fetch"], Daemon::Unreachable, &[]);
    same(&reference, &unreachable, "a daemon that is not there");
    let cloud_off = c.run(&["fetch"], Daemon::Unreachable, &["asc.cloud=off"]);
    same(&reference, &cloud_off, "asc.cloud=off");
    let branches = c.run(&["fetch"], Daemon::Unreachable, &["asc.branchless=false"]);
    same(&reference, &branches, "asc.branchless=false");
    assert!(!c.marker().exists(), "no daemon answered, so no ask is left");
}

#[test]
fn an_unreachable_server_fetch_is_the_same_with_or_without_a_daemon() {
    // Nothing listens on port 1.
    let c = Client::new("asc+http://127.0.0.1:1/acme/app");
    let reference = c.run(&["fetch"], Daemon::Off, &[]);
    let unreachable = c.run(&["fetch"], Daemon::Unreachable, &[]);
    same(&reference, &unreachable, "an unreachable server");
    assert!(!c.marker().exists());
}
