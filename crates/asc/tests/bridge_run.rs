//! P01b acceptance: `Bridge.Run` (SPEC §24.4.5.1). The bridge, not the host, validates the argv
//! of a verb against the embedded schema, spawns `asc <verb> --json …` with a rebuilt
//! environment and a null stdin, and streams `RunEvent`s. The injection corpus never starts a
//! child: a refused call ends with an error code and no reply.
//!
//! The stack verbs that print a report of their own (`sync`, `restack`, `abandon`, ...) end with
//! a typed `OpResult` too; `sl` and `op log` have none and are refused before a child starts.
//! `create`, `modify`, `reword`, `abort` and `stack rename` print their own `OpResult`.

mod support;

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};

use asc_rpc::server::wire::{
    BridgeFrame, Call, RunEvent, RunRequest, bridge_frame::F, run_event::Event,
};
use prost_reflect::prost::Message as _;
use support::{Sandbox, text};

const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 3;
const FAILED_PRECONDITION: i32 = 9;
const PERMISSION_DENIED: i32 = 7;

struct Bridge {
    child: Child,
}

impl Bridge {
    fn start(sb: &Sandbox, cwd: &Path) -> Self {
        let mut cmd = sb.asc(cwd);
        cmd.args(["api", "--stdio"])
            .env("ASC_NO_DAEMON", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd.spawn().expect("spawn asc api --stdio");
        // A bridge that stops answering fails the test instead of hanging it.
        let pid = child.id().to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(90));
            let _ = if cfg!(windows) {
                std::process::Command::new("taskkill")
                    .args(["/F", "/PID", &pid])
                    .output()
            } else {
                std::process::Command::new("kill")
                    .args(["-9", &pid])
                    .output()
            };
        });
        Self { child }
    }

    fn run(&mut self, id: u32, verb: &str, args: &[&str]) {
        let req = RunRequest {
            verb: verb.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            root: String::new(),
        };
        let frame = BridgeFrame {
            id,
            f: Some(F::Call(Call {
                method: "asc.v1.Bridge/Run".to_owned(),
                request: req.encode_to_vec().into(),
                ..Call::default()
            })),
        };
        let body = frame.encode_to_vec();
        let mut wire = (body.len() as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(&body);
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(&wire).unwrap();
        stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Option<BridgeFrame> {
        let out = self.child.stdout.as_mut().unwrap();
        let mut len = [0u8; 4];
        out.read_exact(&mut len).ok()?;
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        out.read_exact(&mut body).ok()?;
        Some(BridgeFrame::decode(body.as_slice()).expect("a frame"))
    }

    /// The events and the closing code of call `id`.
    fn finish(&mut self, id: u32) -> (Vec<Event>, i32, String) {
        let mut events = Vec::new();
        loop {
            let Some(f) = self.recv() else {
                let _ = self.child.kill();
                panic!("the bridge closed before End");
            };
            assert_eq!(f.id, id, "frame for another call");
            match f.f {
                Some(F::Reply(b)) => {
                    let e = RunEvent::decode(b.as_ref()).expect("a RunEvent");
                    events.push(e.event.expect("an event"));
                }
                Some(F::End(e)) => return (events, e.code, e.message),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn close(mut self) {
        drop(self.child.stdin.take());
        let out = self.child.wait_with_output().expect("wait");
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
}

fn workspace(sb: &Sandbox) -> std::path::PathBuf {
    let a = sb.dir("a");
    let out = sb.run(&a, &["init", "-q"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    a
}

fn exit_of(events: &[Event]) -> Option<i32> {
    events.iter().find_map(|e| match e {
        Event::Exit(c) => Some(*c),
        _ => None,
    })
}

#[test]
fn an_allowlisted_verb_runs_and_ends_with_its_exit_code() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a);
    // `up` prints an `OpResult`, which the bridge decodes as it is.
    b.run(1, "up", &[]);
    let (events, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    assert!(exit_of(&events).is_some_and(|c| c < 128), "{events:?}");
    assert!(matches!(events.last(), Some(Event::Exit(_))));
    b.close();
}

#[test]
fn positionals_follow_end_of_options_and_the_verb_accepts_it() {
    // A7: the child argv is `up --json --end-of-options 1`.
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a);
    b.run(1, "up", &["1"]);
    let (events, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    let exit = exit_of(&events).expect("an exit event");
    // The verb's own "nothing above" code is fine; a usage error (128, 129) is not.
    assert!(exit < 128, "{events:?}");
    b.close();
}

#[test]
fn a_failing_verb_still_ends_the_call_with_its_exit_code() {
    let sb = Sandbox::new();
    let plain = sb.dir("plain");
    let mut b = Bridge::start(&sb, &plain);
    // Not an asc workspace: the child fails, the call itself is not an error.
    b.run(1, "sync", &[]);
    let (events, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    assert_ne!(exit_of(&events), Some(0), "{events:?}");
    b.close();
}

#[test]
fn the_injection_corpus_never_starts_a_child() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a);
    let outside = sb.tmp.path().join("outside.txt");
    let outside = outside.to_string_lossy().into_owned();
    let corpus: Vec<(&str, Vec<&str>, i32)> = vec![
        ("create", vec!["-c"], PERMISSION_DENIED),
        ("create", vec!["-C"], PERMISSION_DENIED),
        ("create", vec!["-c=core.pager=sh"], PERMISSION_DENIED),
        ("modify", vec!["--exec=touch x"], PERMISSION_DENIED),
        ("modify", vec!["--exec-path=/x"], PERMISSION_DENIED),
        ("push", vec!["--force"], PERMISSION_DENIED),
        ("push", vec!["--force-with-lease=x"], PERMISSION_DENIED),
        ("fetch", vec!["--upload-pack=sh"], PERMISSION_DENIED),
        ("sync", vec!["--git-dir=/x"], PERMISSION_DENIED),
        // Aliases, external commands, prefixes and unlisted verbs.
        ("ci", vec![], PERMISSION_DENIED),
        ("cre", vec![], PERMISSION_DENIED),
        ("!sh", vec![], PERMISSION_DENIED),
        ("log", vec![], PERMISSION_DENIED),
        ("config", vec!["alias.x=!sh"], PERMISSION_DENIED),
        ("sync --foo", vec![], PERMISSION_DENIED),
        ("", vec![], PERMISSION_DENIED),
        // `--` and `-x` after it, values that start with `-`, unknown flags.
        ("switch", vec!["--", "-x"], INVALID_ARGUMENT),
        ("switch", vec!["-x"], INVALID_ARGUMENT),
        ("switch", vec!["--detach", "--", "main"], INVALID_ARGUMENT),
        ("move", vec!["--onto=-x"], INVALID_ARGUMENT),
        ("move", vec!["--onto", "main"], INVALID_ARGUMENT),
        ("sync", vec!["--end-of-options"], INVALID_ARGUMENT),
        ("sync", vec!["--json"], INVALID_ARGUMENT),
        ("sync", vec!["--no-such-flag"], INVALID_ARGUMENT),
        // Stdin attempts: there is no such flag, and `-` is not a value.
        ("create", vec!["--stdin"], INVALID_ARGUMENT),
        ("stash push", vec!["-"], INVALID_ARGUMENT),
        // Paths and temporary files outside their directories.
        ("create", vec!["../outside.txt"], INVALID_ARGUMENT),
        ("create", vec![outside.as_str()], INVALID_ARGUMENT),
        ("reorder", vec!["--todo=/etc/passwd"], INVALID_ARGUMENT),
        ("split", vec!["--hunks=relative.json"], INVALID_ARGUMENT),
        // Limits.
        ("up", vec!["1", "2"], INVALID_ARGUMENT),
        ("up", vec!["101"], INVALID_ARGUMENT),
    ];
    let mut id = 1;
    for (verb, args, want) in corpus {
        b.run(id, verb, &args);
        let (events, code, msg) = b.finish(id);
        assert!(events.is_empty(), "{verb} {args:?}: {events:?}");
        assert_eq!(code, want, "{verb} {args:?}: {msg}");
        id += 1;
    }
    // An oversized message is refused before any child starts.
    let big = format!("-m={}", "x".repeat(65537));
    b.run(id, "reword", &[big.as_str()]);
    let (events, code, _) = b.finish(id);
    assert!(events.is_empty());
    assert_eq!(code, INVALID_ARGUMENT);
    b.close();
}

#[test]
fn more_calls_than_children_queue_and_all_finish() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a);
    for id in 1..=7 {
        b.run(id, "up", &[]);
    }
    let mut done = std::collections::BTreeSet::new();
    while done.len() < 7 {
        let f = b.recv().expect("a frame");
        if let Some(F::End(e)) = f.f {
            assert_eq!(e.code, OK, "{}", e.message);
            done.insert(f.id);
        }
    }
    b.close();
}

/// The call's `OpResult` event, decoded and printed in the asc JSON profile.
fn op_result_of(events: &[Event]) -> serde_json::Value {
    let bytes = events
        .iter()
        .find_map(|e| match e {
            Event::OpResult(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no op_result event in {events:?}"));
    let pool = prost_reflect::DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let desc = pool.get_message_by_name("asc.v1.OpResult").unwrap();
    let msg = prost_reflect::DynamicMessage::decode(desc, bytes.as_ref()).expect("an OpResult");
    asc_rpc::json::encode(&msg).expect("the profile prints it")
}

fn configure_identity(sb: &Sandbox) {
    std::fs::write(
        sb.user.join(".gitconfig"),
        "[user]\n\tname = T\n\temail = t@example.com\n",
    )
    .unwrap();
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = sb.run(cwd, args);
    assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

/// A workspace on `feat` with one change on top of `master`.
fn stacked(sb: &Sandbox) -> std::path::PathBuf {
    configure_identity(sb);
    let a = workspace(sb);
    std::fs::write(a.join("a.txt"), "a\n").unwrap();
    ok(sb, &a, &["add", "a.txt"]);
    ok(sb, &a, &["commit", "-m", "base"]);
    ok(sb, &a, &["switch", "-c", "feat"]);
    std::fs::write(a.join("b.txt"), "b\n").unwrap();
    ok(sb, &a, &["add", "b.txt"]);
    ok(sb, &a, &["create", "-m", "one"]);
    a
}

#[test]
fn sync_ends_with_a_typed_op_result() {
    // `sync --json` prints its own report; the bridge maps it to the `OpResult` of SPEC §24.2.
    let sb = Sandbox::new();
    let a = stacked(&sb);
    let mut b = Bridge::start(&sb, &a);
    b.run(1, "sync", &[]);
    let (events, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    assert_eq!(exit_of(&events), Some(0), "{events:?}");
    let r = op_result_of(&events);
    assert_eq!(r["version"], 1, "{r}");
    assert_eq!(r["status"], "nothing", "{r}");
    assert!(matches!(events.last(), Some(Event::Exit(0))));
    b.close();
}

#[test]
fn sync_reports_the_trunk_it_fast_forwarded() {
    let sb = Sandbox::new();
    let a = stacked(&sb);
    let git = |dir: &Path, args: &[&str]| {
        let out = sb.command("git", dir).args(args).output().expect("git");
        assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    };
    let bare = sb.dir("remote.git");
    let other = sb.dir("other");
    git(&bare, &["init", "-q", "--bare", "-b", "master"]);
    let url = bare.to_str().unwrap().to_owned();
    ok(&sb, &a, &["remote", "add", "origin", &url]);
    ok(&sb, &a, &["push", "origin", "master"]);
    git(&other, &["clone", "-q", &url, "."]);
    std::fs::write(other.join("up.txt"), "up\n").unwrap();
    git(&other, &["add", "up.txt"]);
    git(&other, &["commit", "-q", "-m", "upstream"]);
    git(&other, &["push", "-q", "origin", "HEAD:master"]);

    let mut b = Bridge::start(&sb, &a);
    b.run(1, "sync", &[]);
    let (events, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    assert_eq!(exit_of(&events), Some(0), "{events:?}");
    let r = op_result_of(&events);
    assert_eq!(r["status"], "ok", "{r}");
    let moved = &r["refs"][0];
    assert_eq!(moved["name"], "refs/heads/master", "{r}");
    assert_eq!(moved["new"].as_str().map(str::len), Some(40), "{r}");
    assert_ne!(moved["old"], moved["new"], "{r}");
    b.close();
}

#[test]
fn stack_verbs_with_reports_of_their_own_end_with_an_op_result() {
    let sb = Sandbox::new();
    let a = stacked(&sb);
    let mut b = Bridge::start(&sb, &a);
    // (verb, args, expected status): `restack` prints `rewrites`, `stack freeze` a `frozen`
    // flag and `abandon` the `verb`/`changes` report.
    let calls: [(&str, &[&str], &str); 3] = [
        ("restack", &[], "nothing"),
        ("stack freeze", &[], "ok"),
        ("abandon", &[], "ok"),
    ];
    for (id, (verb, args, status)) in (1..).zip(calls) {
        b.run(id, verb, args);
        let (events, code, msg) = b.finish(id);
        assert_eq!(code, OK, "{verb}: {msg}");
        assert_eq!(exit_of(&events), Some(0), "{verb}: {events:?}");
        let r = op_result_of(&events);
        assert_eq!(r["status"], status, "{verb}: {r}");
    }
    b.close();
}

#[test]
fn sl_and_op_log_are_refused_before_a_child_starts() {
    // Neither prints an `OpResult`; `sl` is read with `Workspace.Stacks`, and `RunEvent` has no
    // field for an `OpLogResponse`.
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a);
    for (id, verb) in (1..).zip(["sl", "op log"]) {
        b.run(id, verb, &[]);
        let (events, code, msg) = b.finish(id);
        assert!(events.is_empty(), "{verb}: {events:?}");
        assert_eq!(code, FAILED_PRECONDITION, "{verb}: {msg}");
    }
    b.close();
}

#[test]
fn create_modify_reword_and_stack_rename_end_with_an_op_result() {
    let sb = Sandbox::new();
    let a = stacked(&sb);
    std::fs::write(a.join("b.txt"), "bb
").unwrap();
    ok(&sb, &a, &["add", "b.txt"]);
    let mut b = Bridge::start(&sb, &a);
    // `-m=` keeps the value one word; `stack rename` takes the new name after the flags.
    let calls: [(&str, &[&str]); 4] = [
        ("modify", &[]),
        ("reword", &["-m=two"]),
        ("create", &["-m=three"]),
        ("stack rename", &["renamed"]),
    ];
    for (id, (verb, args)) in (1..).zip(calls) {
        b.run(id, verb, args);
        let (events, code, msg) = b.finish(id);
        assert_eq!(code, OK, "{verb}: {msg}");
        assert_eq!(exit_of(&events), Some(0), "{verb}: {events:?}");
        let r = op_result_of(&events);
        assert_eq!(r["status"], "ok", "{verb}: {r}");
        assert!(r["op"].as_str().is_some_and(|o| !o.is_empty()), "{verb}: {r}");
    }
    b.close();
}

#[test]
fn abort_with_nothing_to_abort_ends_with_its_exit_code_and_no_result() {
    let sb = Sandbox::new();
    let a = stacked(&sb);
    let mut b = Bridge::start(&sb, &a);
    b.run(1, "abort", &[]);
    let (events, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    assert_ne!(exit_of(&events), Some(0), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::OpResult(_))), "{events:?}");
    b.close();
}
