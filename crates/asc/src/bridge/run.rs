//! `Bridge.Run` (SPEC §24.2, §24.4.5.1, P01b): runs one allowlisted asc verb as a child process
//! and streams `RunEvent`s: stderr lines, the typed `OpResult`, then the exit code.
//!
//! The argv comes from `argv_schema::build` and nowhere else. The child gets a null stdin, an
//! environment rebuilt from a fixed list (`ASC_NO_ALIASES=1`, `GIT_TERMINAL_PROMPT=0`,
//! `GIT_EDITOR=true`, `GCM_INTERACTIVE=never`, no inherited `GIT_*`), at most
//! [`MAX_CHILDREN`] children run at once (the rest queue), stderr is kept to [`STDERR_CAP`]
//! and cancelling the call drops the child, which kills it.
//!
//! TODO(M02): `cli.proto` (`OpResult`) is not in the descriptor set yet. Until it is, the
//! child's JSON line cannot be decoded, so no `op_result` event is sent and the host sees the
//! `exit` event only. The decode below starts working when the message appears.
//! TODO(P45): verbs without a `--json` form print human text; the decode then fails with
//! `INTERNAL` once `OpResult` exists.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use asc_rpc::server::wire::{Call, RunEvent, RunRequest, run_event};
use prost_reflect::prost::Message as _;
use prost_reflect::{DescriptorPool, MessageDescriptor};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Semaphore;
use tonic::Status;

use super::argv_schema::{self, Refusal, Scope};
use super::{MAX_ROOT, Out, send_reply};

/// Most children at once per bridge.
const MAX_CHILDREN: usize = 4;
/// Most stderr bytes forwarded per child; the rest is read and dropped.
const STDERR_CAP: usize = 1 << 20;
/// Longest forwarded stderr line.
const LINE_CAP: usize = 16 * 1024;
/// Largest result line read from a child's stdout.
const STDOUT_CAP: usize = 8 << 20;
/// Environment variables copied from the bridge to the child (nothing else is).
const KEEP: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SystemRoot",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ComSpec",
    "PATHEXT",
    "USERNAME",
    "USER",
    "LANG",
    "LC_ALL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    // ssh remotes; the socket path is not a secret.
    "SSH_AUTH_SOCK",
    // Where asc keeps its state and which server it talks to, as the user configured them.
    "ASC_HOME",
    "ASC_REMOTE",
    "ASC_SERVER",
    "ASC_SERVER_URL",
    "ASC_TOKEN",
    "ASC_NO_DAEMON",
    "ASC_DAEMON_ENDPOINT",
    "ASC_HOST_CACHE",
    "ASC_BRANCHLESS",
    "ASC_ALLOW_LOOPBACK_HTTP",
];

fn slots() -> &'static Arc<Semaphore> {
    static S: OnceLock<Arc<Semaphore>> = OnceLock::new();
    S.get_or_init(|| Arc::new(Semaphore::new(MAX_CHILDREN)))
}

fn op_result_desc() -> Option<MessageDescriptor> {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap_or_default())
        .get_message_by_name("asc.v1.OpResult")
}

fn refuse(r: Refusal) -> Status {
    match r {
        Refusal::Denied(m) => Status::permission_denied(m),
        Refusal::Invalid(m) => Status::invalid_argument(m),
    }
}

/// The canonical workspace root the child runs in.
fn root_of(own: &Path, request: &str, call: &str) -> Result<PathBuf, Status> {
    let text = if request.is_empty() { call } else { request };
    let path = if text.is_empty() {
        own.to_path_buf()
    } else {
        if text.len() > MAX_ROOT || text.contains('\0') || !Path::new(text).is_absolute() {
            return Err(Status::invalid_argument("root is not an absolute path"));
        }
        PathBuf::from(text)
    };
    path.canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| Status::invalid_argument("root is not a directory"))
}

/// The child's environment, built from [`KEEP`] and the fixed values.
fn environment() -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    for name in KEEP {
        if let Some(v) = std::env::var_os(name) {
            env.push(((*name).into(), v));
        }
    }
    for (k, v) in [
        ("ASC_NO_ALIASES", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_EDITOR", "true"),
        ("GCM_INTERACTIVE", "never"),
        ("GIT_LITERAL_PATHSPECS", "1"),
    ] {
        env.push((k.into(), v.into()));
    }
    env
}

async fn event(out: &Out, id: u32, e: run_event::Event) -> bool {
    let msg = RunEvent { event: Some(e) };
    send_reply(out, id, Bytes::from(msg.encode_to_vec())).await
}

/// Reads `r` to its end and forwards its lines until `cap` bytes were sent.
async fn forward_lines<R: AsyncRead + Unpin>(mut r: R, out: &Out, id: u32, cap: usize) {
    let mut sent = 0usize;
    let mut line: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' {
                if !skipping {
                    emit(out, id, &mut line, &mut sent, cap).await;
                }
                line.clear();
                skipping = false;
            } else if !skipping {
                if line.len() >= LINE_CAP {
                    skipping = true;
                    emit(out, id, &mut line, &mut sent, cap).await;
                    line.clear();
                } else {
                    line.push(b);
                }
            }
        }
    }
    if !line.is_empty() && !skipping {
        emit(out, id, &mut line, &mut sent, cap).await;
    }
}

async fn emit(out: &Out, id: u32, line: &mut Vec<u8>, sent: &mut usize, cap: usize) {
    if *sent >= cap {
        return;
    }
    let text: String = String::from_utf8_lossy(line)
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    *sent += text.len() + 1;
    let _ = event(out, id, run_event::Event::StderrLine(text)).await;
}

/// Reads up to `cap` bytes; `None` when the child wrote more.
async fn read_capped<R: AsyncRead + Unpin>(r: R, cap: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut limited = r.take(cap as u64 + 1);
    let _ = limited.read_to_end(&mut buf).await;
    // Keep draining so the child is never blocked on a full pipe.
    let mut rest = limited.into_inner();
    let mut sink = [0u8; 8192];
    while matches!(rest.read(&mut sink).await, Ok(n) if n > 0) {}
    (buf.len() <= cap).then_some(buf)
}

/// Encodes the child's JSON result as an `OpResult`, when the message exists.
fn op_result(stdout: &[u8]) -> Result<Option<Vec<u8>>, Status> {
    let Some(desc) = op_result_desc() else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(stdout);
    let Some(last) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(last)
        .map_err(|_| Status::internal("the verb printed a result that is not JSON"))?;
    let msg = asc_rpc::json::decode(&desc, &value)
        .map_err(|_| Status::internal("the verb printed a result that is not an OpResult"))?;
    Ok(Some(msg.encode_to_vec()))
}

/// `Bridge.Run`: validates, queues, spawns, streams.
///
/// # Errors
/// `PERMISSION_DENIED` or `INVALID_ARGUMENT` when a rule of the argv schema is broken,
/// `UNAVAILABLE` when the child cannot start, `INTERNAL` for an undecodable result.
pub async fn serve(own: PathBuf, id: u32, call: Call, out: &Out) -> Result<(), Status> {
    let req = RunRequest::decode(call.request.as_ref())
        .map_err(|_| Status::invalid_argument("RunRequest does not decode"))?;
    let root = root_of(&own, &req.root, &call.root)?;
    let tmp = std::env::var_os("ASC_PLUGIN_TMP")
        .map(PathBuf::from)
        .and_then(|p| p.canonicalize().ok());
    let argv = argv_schema::build(
        &req.verb,
        &req.args,
        &Scope {
            root: &root,
            tmp: tmp.as_deref(),
        },
    )
    .map_err(refuse)?;
    let exe =
        std::env::current_exe().map_err(|_| Status::unavailable("cannot find the asc binary"))?;

    let _slot = slots()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Status::unavailable("the bridge is stopping"))?;

    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(&argv)
        .current_dir(&root)
        .env_clear()
        .envs(environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping the call (a host cancel) kills the child.
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd
        .spawn()
        .map_err(|_| Status::unavailable("cannot start the asc child"))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(Status::internal("the child has no pipes"));
    };
    let (captured, (), status) = tokio::join!(
        read_capped(stdout, STDOUT_CAP),
        forward_lines(stderr, out, id, STDERR_CAP),
        child.wait()
    );
    let code = status
        .map_err(|_| Status::internal("cannot wait for the asc child"))?
        .code()
        .unwrap_or(-1);
    // Exit 1 (paused on a conflict) and 2 (ambiguous) carry an OpResult; the others may not.
    if let Some(bytes) = captured {
        match op_result(&bytes) {
            Ok(Some(encoded)) => {
                event(out, id, run_event::Event::OpResult(encoded.into())).await;
            }
            Ok(None) => {}
            Err(s) if matches!(code, 0..=2) => return Err(s),
            Err(_) => {}
        }
    } else {
        return Err(Status::resource_exhausted("the verb's result is too large"));
    }
    event(out, id, run_event::Event::Exit(code)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_a_fixed_set() {
        let env = environment();
        let fixed = [
            "ASC_NO_ALIASES",
            "GIT_TERMINAL_PROMPT",
            "GIT_EDITOR",
            "GCM_INTERACTIVE",
            "GIT_LITERAL_PATHSPECS",
        ];
        for (k, _) in &env {
            let k = k.to_string_lossy();
            assert!(
                KEEP.contains(&k.as_ref()) || fixed.contains(&k.as_ref()),
                "{k}"
            );
        }
        let get = |n: &str| env.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        assert_eq!(get("ASC_NO_ALIASES").as_deref(), Some("1".as_ref()));
        assert_eq!(get("GIT_EDITOR").as_deref(), Some("true".as_ref()));
        assert_eq!(get("GCM_INTERACTIVE").as_deref(), Some("never".as_ref()));
        assert!(KEEP.iter().all(|k| !k.starts_with("GIT_")));
    }
}
