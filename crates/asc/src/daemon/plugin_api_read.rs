//! `Workspace.ReadFile` (SPEC §7, §24.4.2; P02): a file at `rev:path`, a `FileId` or a stage
//! (0-3), streamed from the local store. It reads the object store and the staging rows, never the
//! work tree, so it works offline and costs a virtual workspace no hydration (IP-D14). An object
//! the store lacks is fetched through the tiers by the child's object stack.
//!
//! Like the other plugin RPCs, the work is done by this binary run as a child in the mounted
//! workspace (`asc ws-api read`, see `stack/cli/ws_api.rs`). The child prints an 8-byte big-endian
//! total size and then the requested range as raw bytes; this module cuts the bytes into
//! messages. Authorisation is that of the other plugin RPCs: the workspace must be mounted and its
//! root is verified before the child starts.
//!
//! Bounds: the revision and path are bounded text, a `FileId` is exactly 32 bytes, a message is at
//! most 64 KiB of data, one call streams at most 256 MiB (ask for the rest with `offset`), and the
//! child runs at most five minutes.

use std::ffi::OsString;
use std::time::Duration;

use asc_rpc::server::wire;
use bytes::Bytes;
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, BufReader};
use tonic::Status;

use super::plugin_api::{PluginStream, bad, command, internal, mounted, os, path_args, text_arg};
use super::service::State;
use futures::StreamExt as _;

/// Data bytes in one response message.
const CHUNK: usize = 64 * 1024;
/// Most bytes one call streams.
pub(super) const MAX_READ: u64 = 256 * 1024 * 1024;
/// The child's run limit.
const RUN_LIMIT: Duration = Duration::from_secs(300);
/// Most stderr bytes kept for an error message.
const MAX_ERR: u64 = 64 * 1024;
/// The marker the child prints for a path or object that does not exist.
const NOT_FOUND: &str = "not found:";

/// Lowercase hex of `b`.
fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// The child's arguments for a request, validated.
fn child_args(r: &wire::WsReadFileRequest) -> Result<Vec<OsString>, Status> {
    use wire::ws_read_file_request::At;
    let mut args = os(&["ws-api", "read"]);
    match &r.at {
        Some(At::RevPath(rp)) => {
            if !rp.rev.is_empty() {
                args.push("--rev".into());
                args.push(text_arg(&rp.rev, "the revision", true)?.into());
            }
            args.push("--path".into());
            args.extend(one_path(&rp.path)?);
        }
        Some(At::FileId(id)) => {
            if id.len() != 32 {
                return Err(bad("a file id is 32 bytes"));
            }
            args.push("--file-id".into());
            args.push(hex(id).into());
        }
        Some(At::Stage(sp)) => {
            if sp.stage > 3 {
                return Err(bad("a stage is 0 to 3"));
            }
            args.push("--stage".into());
            args.push(sp.stage.to_string().into());
            args.push("--path".into());
            args.extend(one_path(&sp.path)?);
        }
        None => return Err(bad("name rev_path, file_id or stage")),
    }
    args.push("--offset".into());
    args.push(r.offset.to_string().into());
    args.push("--length".into());
    args.push(r.length.min(MAX_READ).to_string().into());
    Ok(args)
}

fn one_path(raw: &Bytes) -> Result<Vec<OsString>, Status> {
    let p = path_args(std::slice::from_ref(raw))?;
    if p.len() != 1 || raw.is_empty() {
        return Err(bad("a file read needs one path"));
    }
    Ok(p)
}

fn failure(code: Option<i32>, stderr: &[u8]) -> Status {
    let text = String::from_utf8_lossy(stderr);
    let text: String = text.trim().chars().take(512).collect();
    if text.contains(NOT_FOUND) {
        return Status::not_found(text);
    }
    Status::failed_precondition(format!(
        "asc ws-api read failed ({}): {text}",
        code.map_or("signal".into(), |c| c.to_string())
    ))
}

async fn pump(
    mut child: tokio::process::Child,
    tx: &tokio::sync::mpsc::Sender<Result<wire::ReadFileResponse, Status>>,
) -> Result<(), Status> {
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(internal("the child has no pipes"));
    };
    let errors = tokio::spawn(async move {
        let mut b = Vec::new();
        let _ = err.take(MAX_ERR).read_to_end(&mut b).await;
        b
    });
    let deadline = tokio::time::Instant::now() + RUN_LIMIT;
    let mut out = BufReader::new(out);
    let mut head = [0u8; 8];
    let mut sent = 0u64;
    let work = async {
        let got = out.read_exact(&mut head).await;
        let total = match got {
            Ok(_) => Some(u64::from_be_bytes(head)),
            Err(_) => None,
        };
        let mut buf = vec![0u8; CHUNK];
        let mut any = false;
        if let Some(total) = total {
            loop {
                let n = out.read(&mut buf).await.map_err(internal)?;
                if n == 0 {
                    break;
                }
                sent += n as u64;
                if sent > MAX_READ {
                    return Err(Status::resource_exhausted(format!(
                        "a call streams at most {MAX_READ} bytes; use offset"
                    )));
                }
                any = true;
                let msg = wire::ReadFileResponse {
                    data: Bytes::copy_from_slice(&buf[..n]),
                    total_size: total,
                };
                if tx.send(Ok(msg)).await.is_err() {
                    return Ok(None);
                }
            }
            if !any {
                let msg = wire::ReadFileResponse {
                    data: Bytes::new(),
                    total_size: total,
                };
                if tx.send(Ok(msg)).await.is_err() {
                    return Ok(None);
                }
            }
        }
        Ok(Some(total.is_some()))
    };
    let done = match tokio::time::timeout_at(deadline, work).await {
        Ok(r) => r,
        Err(_) => {
            let _ = child.kill().await;
            return Err(Status::deadline_exceeded(format!(
                "asc ws-api read ran longer than {} s",
                RUN_LIMIT.as_secs()
            )));
        }
    };
    let done = match done {
        Ok(Some(d)) => d,
        // The client went away.
        Ok(None) => {
            let _ = child.kill().await;
            return Ok(());
        }
        Err(e) => {
            let _ = child.kill().await;
            return Err(e);
        }
    };
    let status = child.wait().await.map_err(internal)?;
    if !status.success() || !done {
        let stderr = errors.await.unwrap_or_default();
        return Err(failure(status.code(), &stderr));
    }
    Ok(())
}

pub async fn read_file(
    state: &Arc<State>,
    r: wire::WsReadFileRequest,
) -> Result<PluginStream<wire::ReadFileResponse>, Status> {
    let m = mounted(state, &r.ws_id)?;
    let args = child_args(&r)?;
    let mut c = command(&m, &args)?;
    c.stdin(std::process::Stdio::null());
    let child = c
        .spawn()
        .map_err(|e| internal(format!("cannot run asc ws-api: {e}")))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<wire::ReadFileResponse, Status>>(4);
    tokio::spawn(async move {
        if let Err(status) = pump(child, &tx).await {
            let _ = tx.send(Err(status)).await;
        }
    });
    Ok(
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
            .boxed(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::ws_read_file_request::At;

    fn req(at: Option<At>) -> wire::WsReadFileRequest {
        wire::WsReadFileRequest {
            ws_id: String::new(),
            at,
            offset: 0,
            length: 0,
        }
    }

    #[test]
    fn requests_are_validated() {
        assert!(child_args(&req(None)).is_err());
        assert!(child_args(&req(Some(At::FileId(Bytes::from_static(b"short"))))).is_err());
        let ok = child_args(&req(Some(At::FileId(Bytes::from(vec![0xab; 32]))))).unwrap();
        assert!(ok.iter().any(|a| a.to_string_lossy() == "ab".repeat(32)));
        let bad_stage = wire::WsStagePath {
            stage: 4,
            path: Bytes::from_static(b"a"),
        };
        assert!(child_args(&req(Some(At::Stage(bad_stage)))).is_err());
        let dash = wire::WsRevPath {
            rev: "-x".into(),
            path: Bytes::from_static(b"a"),
        };
        assert!(child_args(&req(Some(At::RevPath(dash)))).is_err());
        let empty = wire::WsRevPath {
            rev: String::new(),
            path: Bytes::new(),
        };
        assert!(child_args(&req(Some(At::RevPath(empty)))).is_err());
        let up = wire::WsRevPath {
            rev: "HEAD".into(),
            path: Bytes::from_static(b"../x"),
        };
        assert!(child_args(&req(Some(At::RevPath(up)))).is_err());
    }
}
