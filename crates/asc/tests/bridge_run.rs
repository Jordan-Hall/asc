//! P01b acceptance: `Bridge.Run` (SPEC §24.4.5.1). The bridge, not the host, validates the argv
//! of a verb against the embedded schema, spawns `asc <verb> --json …` with a rebuilt
//! environment and a null stdin, and streams `RunEvent`s. The injection corpus never starts a
//! child: a refused call ends with an error code and no reply.
//!
//! TODO(P45): `create`, `modify` and others still lack their `--json` form, so the positive
//! tests use `up`, which prints an `OpResult`.

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
    // `up` prints an `OpResult`, which the bridge decodes; `sync` prints its own report shape.
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
