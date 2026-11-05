//! `asc cache go` (docs/designs/build-cache.md §4.5): the go command's `GOCACHEPROG`. The go
//! command keys each compile, link and test result itself (its action id); asc keeps the result
//! in the shared store, so every work tree and the server tier see it. The protocol is JSON lines
//! over stdin and stdout: a `put` carries the body as a base64 JSON string on the next line, and
//! each reply names a file holding the body, here the store's own blob.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use asc_buildcache::{Action, ActionKey, Output, OutputKind, Store, hex, unhex};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::remote::{self, Fetched, Mode};
use super::run::{origin, spawn_push};
use super::{max_size, open_store};
use crate::cli::ci::cache::{HttpApi, http_api};
use crate::cli::common::{CliError, Ctx, Result};

/// The tool name of go entries.
pub const TOOL: &str = "go";
/// The size of the go command's action and output ids.
const ID: usize = 32;

#[derive(clap::Args, Debug)]
pub struct GoArgs {
    /// Neither fetch from nor queue for the server.
    #[arg(long = "no-remote")]
    no_remote: bool,
}

/// One request of the go command.
#[derive(Deserialize, Debug)]
struct Request {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "Command")]
    command: String,
    /// Base64, as Go encodes `[]byte`.
    #[serde(rename = "ActionID", default)]
    action_id: Option<String>,
    #[serde(rename = "OutputID", default)]
    output_id: Option<String>,
    #[serde(rename = "BodySize", default)]
    body_size: i64,
}

/// A 32-byte id sent as base64.
fn id(field: Option<&str>, name: &str) -> std::result::Result<[u8; ID], String> {
    let bytes = STANDARD
        .decode(field.unwrap_or_default())
        .map_err(|e| format!("{name}: {e}"))?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| format!("{name} has {} bytes, not {ID}", b.len()))
}

fn key(action: &[u8; ID]) -> ActionKey {
    ActionKey::builder(TOOL).part("action", action).finish()
}

fn disk_path(store: &Store, blob: &[u8; 32]) -> String {
    let p = store.blob_path(blob);
    std::path::absolute(&p)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// The reply to a `get` from an entry: the output id is the output's path, the body its blob.
fn found(store: &Store, rid: i64, action: &Action) -> Option<Value> {
    let [o] = action.outputs.as_slice() else {
        return None;
    };
    let out = unhex(&o.path)?;
    let blob = unhex(&o.blob)?;
    if !store.has_blob(&blob) {
        return None;
    }
    Some(json!({
        "ID": rid,
        "OutputID": STANDARD.encode(out),
        "Size": o.size,
        "DiskPath": disk_path(store, &blob),
    }))
}

fn miss(rid: i64) -> Value {
    json!({ "ID": rid, "Miss": true })
}

fn failed(rid: i64, err: impl std::fmt::Display) -> Value {
    json!({ "ID": rid, "Err": err.to_string() })
}

/// Stores a `put`: the body as a blob, the entry under the action id.
fn put(
    store: &Store,
    origin: &asc_buildcache::Origin,
    req: &Request,
    body: &[u8],
) -> std::result::Result<(ActionKey, PathBuf, u64), String> {
    let action_id = id(req.action_id.as_deref(), "ActionID")?;
    let output_id = id(req.output_id.as_deref(), "OutputID")?;
    let (blob, size) = store.put_blob(body).map_err(|e| e.to_string())?;
    let mut action = Action::new(TOOL, "", origin.clone());
    action.outputs = vec![Output {
        path: hex(&output_id),
        kind: OutputKind::File,
        size,
        blob: hex(&blob),
    }];
    let key = key(&action_id);
    store.put_action(&key, &action).map_err(|e| e.to_string())?;
    Ok((key, PathBuf::from(disk_path(store, &blob)), size))
}

/// The server tier, while it answers: the first failure turns it off for this build.
struct Remote {
    api: HttpApi,
    up: AtomicBool,
}

impl Remote {
    async fn get(&self, store: &Store, rid: i64, key: &ActionKey) -> Value {
        if !self.up.load(Ordering::Relaxed) {
            return miss(rid);
        }
        match remote::fetch(&self.api, store, key).await {
            Ok(Fetched::Stored(_)) => store
                .get_action(key)
                .ok()
                .flatten()
                .and_then(|a| found(store, rid, &a))
                .unwrap_or_else(|| miss(rid)),
            Ok(Fetched::Refused(m)) => {
                eprintln!("warning: build cache artefact refused: {m}");
                miss(rid)
            }
            Ok(Fetched::Missing | Fetched::TooLarge) => miss(rid),
            Err(e) => {
                if self.up.swap(false, Ordering::Relaxed) {
                    eprintln!("warning: build cache server: {e}; going on without it");
                }
                miss(rid)
            }
        }
    }
}

/// Reads one request and, for a `put`, its body.
async fn next(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> Result<Option<(Request, Vec<u8>)>> {
    let line = loop {
        match lines.next_line().await? {
            None => return Ok(None),
            Some(l) if l.trim().is_empty() => {}
            Some(l) => break l,
        }
    };
    let req: Request = serde_json::from_str(&line)
        .map_err(|e| CliError::Fatal(format!("asc cache go: bad request: {e}")))?;
    if req.body_size <= 0 {
        return Ok(Some((req, Vec::new())));
    }
    // The go command ends the request with a blank line before the body.
    let text = loop {
        match lines.next_line().await? {
            None => {
                return Err(CliError::Fatal(
                    "asc cache go: a put ended before its body".into(),
                ));
            }
            Some(l) if l.trim().is_empty() => {}
            Some(l) => break l,
        }
    };
    let body = serde_json::from_str::<String>(&text)
        .ok()
        .and_then(|s| STANDARD.decode(s).ok())
        .filter(|b| b.len() as i64 == req.body_size)
        .ok_or_else(|| CliError::Fatal("asc cache go: a put body is not its size".into()))?;
    Ok(Some((req, body)))
}

pub async fn run(ctx: &Ctx, a: GoArgs) -> Result<ExitCode> {
    let store = open_store(ctx)?;
    let root: &Path = ctx
        .ws_opt()
        .map(|ws| ws.projection.toplevel())
        .unwrap_or(&ctx.cwd);
    let origin = origin(root);
    let mode = if a.no_remote {
        Mode::Off
    } else {
        remote::mode(ctx).unwrap_or(Mode::Off)
    };
    // No server configured is a local cache, as for every other tool.
    let server = match mode {
        Mode::Off => None,
        _ => http_api(ctx, None).await.ok().map(|api| {
            Arc::new(Remote {
                api,
                up: AtomicBool::new(true),
            })
        }),
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = rx.recv().await {
            let mut line = v.to_string().into_bytes();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let _ = tx.send(json!({ "ID": 0, "KnownCommands": ["get", "put", "close"] }));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut queued = false;
    let mut pending = Vec::new();
    while let Some((req, body)) = next(&mut lines).await? {
        let rid = req.id;
        match req.command.as_str() {
            "get" => {
                let action_id = match id(req.action_id.as_deref(), "ActionID") {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = tx.send(failed(rid, e));
                        continue;
                    }
                };
                let key = key(&action_id);
                let local = match store.lookup(TOOL, &key, &origin) {
                    Ok(hit) => hit.and_then(|a| found(&store, rid, &a)),
                    Err(e) => {
                        eprintln!("warning: build cache: {e}");
                        None
                    }
                };
                match (local, &server) {
                    (Some(reply), _) => {
                        if mode == Mode::ReadWrite && store.queue_push(&key).is_ok() {
                            queued = true;
                        }
                        let _ = tx.send(reply);
                    }
                    // A remote fetch is answered when it is done; the go command asks for
                    // other actions meanwhile.
                    (None, Some(server)) => {
                        let (server, store, tx) = (server.clone(), store.clone(), tx.clone());
                        pending.push(tokio::spawn(async move {
                            let _ = tx.send(server.get(&store, rid, &key).await);
                        }));
                    }
                    (None, None) => {
                        let _ = tx.send(miss(rid));
                    }
                }
            }
            "put" => match put(&store, &origin, &req, &body) {
                Ok((key, path, size)) => {
                    if mode == Mode::ReadWrite && store.queue_push(&key).is_ok() {
                        queued = true;
                    }
                    let _ = tx.send(json!({
                        "ID": rid,
                        "Size": size,
                        "DiskPath": path.to_string_lossy(),
                    }));
                }
                Err(e) => {
                    let _ = tx.send(failed(rid, format!("asc cache go: {e}")));
                }
            },
            "close" => {
                for p in pending.drain(..) {
                    let _ = p.await;
                }
                let _ = tx.send(json!({ "ID": rid }));
                break;
            }
            other => {
                let _ = tx.send(failed(rid, format!("unknown command '{other}'")));
            }
        }
    }
    for p in pending {
        let _ = p.await;
    }
    drop(tx);
    let _ = writer.await;
    if queued {
        spawn_push(&store, &ctx.cwd);
    }
    if let Ok(max) = max_size(ctx) {
        let _ = store.maybe_gc(max);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_32_bytes_of_base64() {
        let raw = [7u8; ID];
        assert_eq!(id(Some(&STANDARD.encode(raw)), "ActionID").unwrap(), raw);
        assert!(id(Some(&STANDARD.encode([1u8; 8])), "ActionID").is_err());
        assert!(id(Some("not base64!"), "ActionID").is_err());
        assert!(id(None, "ActionID").is_err());
    }

    #[test]
    fn a_put_is_found_by_a_get_and_names_the_stored_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s")).unwrap();
        let action = [1u8; ID];
        let output = [2u8; ID];
        let req = Request {
            id: 4,
            command: "put".into(),
            action_id: Some(STANDARD.encode(action)),
            output_id: Some(STANDARD.encode(output)),
            body_size: 5,
        };
        let (k, path, size) = put(&store, &Default::default(), &req, b"hello").unwrap();
        assert_eq!((k, size), (key(&action), 5));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let entry = store.get_action(&key(&action)).unwrap().unwrap();
        let reply = found(&store, 9, &entry).unwrap();
        assert_eq!(reply["ID"], 9);
        assert_eq!(reply["OutputID"], STANDARD.encode(output));
        assert_eq!(reply["Size"], 5);
        assert_eq!(reply["DiskPath"], path.to_string_lossy().as_ref());
        // An entry of another tool's shape is not a go hit.
        let mut other = entry.clone();
        other.outputs.push(other.outputs[0].clone());
        assert!(found(&store, 9, &other).is_none());
    }
}
