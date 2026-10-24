//! P01c acceptance: `Bridge.Token` and `Bridge.StoreToken` (SPEC §24.6): `ASC_TOKEN` first, else
//! `git credential fill` that never prompts, `NOT_FOUND`, `NO_CREDENTIAL_HELPER` with nothing
//! written, and the token never leaves the reply frame.
#![cfg(feature = "serve")]
#![allow(clippy::unwrap_used)]

mod support;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

use asc_rpc::server::wire::{
    BridgeFrame, Call, StoreTokenRequest, TokenRequest, TokenResponse, bridge_frame::F,
};
use prost_reflect::prost::Message as _;
use support::{Sandbox, text};

const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 3;
const NOT_FOUND: i32 = 5;
const FAILED_PRECONDITION: i32 = 9;

const HOST: &str = "forge.example.test";
const TOKEN: &str = "canary-S3CR3T-bridge-token";

struct Bridge {
    child: Child,
}

impl Bridge {
    fn start(sb: &Sandbox, cwd: &Path, asc_token: Option<&str>, askpass: Option<&Path>) -> Self {
        let mut cmd = sb.asc(cwd);
        cmd.args(["api", "--stdio"])
            .env("ASC_NO_DAEMON", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(t) = asc_token {
            cmd.env("ASC_TOKEN", t);
        }
        if let Some(a) = askpass {
            cmd.env("GIT_ASKPASS", a).env("SSH_ASKPASS", a);
        }
        let child = cmd.spawn().expect("spawn asc api --stdio");
        let pid = child.id().to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(45));
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

    fn call(&mut self, id: u32, method: &str, request: Vec<u8>) {
        let frame = BridgeFrame {
            id,
            f: Some(F::Call(Call {
                method: method.to_owned(),
                request: request.into(),
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

    fn finish(&mut self, id: u32) -> (Vec<Vec<u8>>, i32, String) {
        let mut replies = Vec::new();
        loop {
            let Some(f) = self.recv() else {
                let _ = self.child.kill();
                panic!("the bridge closed before End");
            };
            assert_eq!(f.id, id);
            match f.f {
                Some(F::Reply(b)) => replies.push(b.to_vec()),
                Some(F::End(e)) => return (replies, e.code, e.message),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn token(&mut self, id: u32, host: &str) -> (Option<String>, i32, String) {
        let req = TokenRequest {
            host: host.to_owned(),
        };
        self.call(id, "asc.v1.Bridge/Token", req.encode_to_vec());
        let (replies, code, msg) = self.finish(id);
        let token = replies
            .first()
            .map(|r| TokenResponse::decode(r.as_slice()).unwrap().token);
        (token, code, msg)
    }

    fn store(&mut self, id: u32, host: &str, token: &str) -> (i32, String) {
        let req = StoreTokenRequest {
            host: host.to_owned(),
            token: token.to_owned(),
        };
        self.call(id, "asc.v1.Bridge/StoreToken", req.encode_to_vec());
        let (replies, code, msg) = self.finish(id);
        assert!(code != OK || replies.len() == 1);
        (code, msg)
    }

    fn close(mut self) -> std::process::Output {
        drop(self.child.stdin.take());
        self.child.wait_with_output().expect("wait")
    }
}

fn sh_path(p: &Path) -> String {
    p.display().to_string().replace(char::from(92), "/")
}

fn workspace(sb: &Sandbox) -> PathBuf {
    let a = sb.dir("a");
    let out = sb.run(&a, &["init", "-q"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    a
}

/// A helper that logs each verb and its input and, for `get`, answers with `answer`.
fn install_helper(sb: &Sandbox, answer: Option<&str>) -> PathBuf {
    let script = sb.tmp.path().join("helper.sh");
    let log = sb.tmp.path().join("helper.log");
    let reply = answer.map_or(String::new(), |t| {
        format!("[ \"$1\" = get ] && printf 'username=asc\\npassword=%s\\n' '{t}'\n")
    });
    let body = format!(
        "#!/bin/sh\n{{ echo \"== $1\"; cat; echo '--'; }} >> '{}'\n{reply}exit 0\n",
        sh_path(&log)
    );
    std::fs::write(&script, body).unwrap();
    std::fs::write(
        sb.user.join(".gitconfig"),
        format!("[credential]\n\thelper = !sh '{}'\n", sh_path(&script)),
    )
    .unwrap();
    log
}

#[test]
fn asc_token_wins_and_a_bad_host_is_refused() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    install_helper(&sb, Some("from-helper"));
    let mut b = Bridge::start(&sb, &a, Some(TOKEN), None);
    let (t, code, msg) = b.token(1, HOST);
    assert_eq!((code, t.as_deref()), (OK, Some(TOKEN)), "{msg}");
    for (id, host) in [
        (2, ""),
        (3, "https://u:p@forge.example.test"),
        (4, "ftp://forge.example.test"),
        (5, "forge.example.test/path"),
    ] {
        let (t, code, _) = b.token(id, host);
        assert_eq!((code, t), (INVALID_ARGUMENT, None), "{host}");
    }
    let out = b.close();
    assert!(!text(&out.stderr).contains(TOKEN));
}

#[test]
fn the_helper_answers_and_nothing_found_is_not_found() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    install_helper(&sb, Some(TOKEN));
    let mut b = Bridge::start(&sb, &a, None, None);
    let (t, code, msg) = b.token(1, HOST);
    assert_eq!((code, t.as_deref()), (OK, Some(TOKEN)), "{msg}");
    b.close();

    let sb = Sandbox::new();
    let a = workspace(&sb);
    install_helper(&sb, None);
    let mut b = Bridge::start(&sb, &a, None, None);
    let (t, code, msg) = b.token(1, HOST);
    assert_eq!((code, t), (NOT_FOUND, None));
    assert!(msg.starts_with("NOT_FOUND"), "{msg}");
    b.close();
}

#[test]
fn token_never_prompts() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    // No helper answers, so git would fall back to asking; the askpass must never run.
    install_helper(&sb, None);
    let mark = sb.tmp.path().join("asked");
    let askpass = sb.tmp.path().join("askpass.sh");
    std::fs::write(
        &askpass,
        format!(
            "#!/bin/sh\necho asked > '{}'\necho prompted\n",
            sh_path(&mark)
        ),
    )
    .unwrap();
    let mut b = Bridge::start(&sb, &a, None, Some(&askpass));
    let (_, code, _) = b.token(1, HOST);
    assert_eq!(code, NOT_FOUND);
    let out = b.close();
    assert!(!mark.exists(), "the askpass program ran");
    assert!(!text(&out.stdout).contains("prompted"));
}

#[test]
fn store_token_without_a_helper_writes_nothing() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a, None, None);
    let (code, msg) = b.store(1, HOST, TOKEN);
    assert_eq!(code, FAILED_PRECONDITION);
    assert!(msg.contains("NO_CREDENTIAL_HELPER"), "{msg}");
    assert!(!msg.contains(TOKEN));
    b.close();

    // The plaintext `store` helper does not count, and no credentials file appears.
    std::fs::write(
        sb.user.join(".gitconfig"),
        "[credential]\n\thelper = store\n",
    )
    .unwrap();
    let mut b = Bridge::start(&sb, &a, None, None);
    let (code, msg) = b.store(1, HOST, TOKEN);
    assert_eq!(code, FAILED_PRECONDITION);
    assert!(msg.contains("NO_CREDENTIAL_HELPER"), "{msg}");
    b.close();
    assert!(!sb.user.join(".git-credentials").exists());
    assert!(!sb.user.join(".config/git/credentials").exists());
}

#[test]
fn store_token_approves_through_the_helper() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let log = install_helper(&sb, None);
    let mut b = Bridge::start(&sb, &a, None, None);
    let (code, msg) = b.store(1, HOST, TOKEN);
    assert_eq!(code, OK, "{msg}");
    let out = b.close();
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("== store"), "{calls}");
    assert!(calls.contains(&format!("host={HOST}")), "{calls}");
    assert!(calls.contains(&format!("password={TOKEN}")), "{calls}");
    assert!(!text(&out.stderr).contains(TOKEN));
    assert!(!text(&out.stdout).contains(TOKEN));
}
