//! `subscriptions/listen` for `asc mcp` over stdio (SPEC 22.3.5, unit M11).
//!
//! A listen is a long request. Its `resourceSubscriptions` name `asc://ws/{ws}/…` resources of the
//! launch workspace, and each matching `Workspace.Watch` event becomes a
//! `notifications/resources/updated` for the URI the client gave:
//!
//! | event | updates |
//! |---|---|
//! | `KIND_PATHS` | the `file/{path}` URIs naming a changed path, `status` and `smartlog` |
//! | `KIND_HEAD` | `status` and `smartlog` |
//! | `KIND_STACK` (C49) | `smartlog` |
//! | `KIND_RESYNC` (events were lost) | every subscribed URI |
//!
//! **Limits.** A process serves at most [`MAX_LISTENS`] listens at once, each with at most
//! [`MAX_URIS`] URIs. The slot is taken before the stream is opened and given back when the listen
//! ends, whatever ended it.
//!
//! **Safety.** A URI is parsed before any stream opens: a foreign workspace, a query or fragment,
//! a malformed escape, and a `file` path the [path gate](super::pathgate) refuses (absolute, `..`,
//! a link on the way, ignored) all fail the whole request, so a notification can never reveal that
//! a path the gate hides has changed. The notification carries only the URI.
//!
//! The protocol layer (`asc-mcp`'s `Handler`) does not route `subscriptions/listen` yet, so
//! [`serve`] is the stdio loop of `asc mcp`: it answers that method here and passes everything else
//! to `Handler::handle_message`.
//! TODO(M12a): move the method into `asc-mcp` beside `resources/*` when its dispatch exists.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asc_mcp::backend::McpBackend;
use asc_mcp::errors::{INVALID_PARAMS, INVALID_REQUEST, INTERNAL_ERROR, RpcError};
use asc_mcp::handler::{Caller, Handler, MAX_MESSAGE_BYTES, Notifier};
use asc_rpc::ipc::WorkspaceClient;
use asc_rpc::server::wire::{self, watch_event::Kind};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tonic::Status;
use tonic::transport::Channel;

use super::backend::LocalBackend;
use super::pathgate::{Disk, Gate};

/// Most listens one process serves at once.
pub const MAX_LISTENS: usize = 2;
/// Most URIs one listen may name.
pub const MAX_URIS: usize = 64;
/// Longest URI accepted.
const MAX_URI: usize = 4096 + 64;

/// What a subscription names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Resource {
    Status,
    Smartlog,
    /// A gated, normalised worktree path (`/` separators).
    File(Vec<u8>),
}

fn bad(msg: impl AsRef<str>) -> RpcError {
    RpcError::new(INVALID_PARAMS, msg)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` once; the result must be UTF-8.
fn unescape(s: &str) -> Result<String, RpcError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1).copied().and_then(hex);
            let l = b.get(i + 2).copied().and_then(hex);
            let (Some(h), Some(l)) = (h, l) else {
                return Err(bad("malformed percent-encoding in a resource URI"));
            };
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad("a resource URI is not valid UTF-8"))
}

/// Parses one `asc://ws/{ws}/…` URI of the launch workspace `ws_id`.
fn parse_uri(uri: &str, ws_id: &str, gate: &Gate) -> Result<Resource, RpcError> {
    if uri.len() > MAX_URI {
        return Err(bad("a resource URI is too long"));
    }
    let Some(rest) = uri.strip_prefix("asc://ws/") else {
        return Err(bad(
            "only asc://ws/{ws}/status, smartlog and file/{path} can be subscribed",
        ));
    };
    if rest.contains(['?', '#']) {
        return Err(bad("a subscribed URI has no query or fragment"));
    }
    let (ws, tail) = rest
        .split_once('/')
        .ok_or_else(|| bad("a subscribed URI names a resource after the workspace"))?;
    if ws != ws_id {
        return Err(bad("the workspace is not the one this server was started in"));
    }
    match tail {
        "status" => Ok(Resource::Status),
        "smartlog" => Ok(Resource::Smartlog),
        _ => {
            let Some(path) = tail.strip_prefix("file/") else {
                return Err(bad(
                    "only asc://ws/{ws}/status, smartlog and file/{path} can be subscribed",
                ));
            };
            let path = unescape(path)?;
            if path.is_empty() || path.ends_with('/') {
                return Err(bad("a file URI names one file"));
            }
            gate.check(&path, Disk::Worktree, false)
                .map_err(|r| bad(r.to_string()))?;
            let parts = super::pathgate::components(&path).map_err(|r| bad(r.to_string()))?;
            Ok(Resource::File(parts.join("/").into_bytes()))
        }
    }
}

/// The process-wide listen slots.
#[derive(Debug, Clone, Default)]
pub struct Listens {
    active: Arc<AtomicUsize>,
}

/// One taken slot; dropping it frees the slot.
#[derive(Debug)]
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// An accepted listen: the URIs it watches and its slot.
#[derive(Debug)]
pub struct Subscription {
    /// Each distinct resource with the URI text the client gave for it.
    entries: BTreeMap<Resource, String>,
    _slot: Slot,
}

impl Listens {
    /// Validates a `subscriptions/listen` request and takes a slot.
    ///
    /// # Errors
    /// -32602 for bad parameters, an unknown or hidden URI, or more than [`MAX_URIS`] URIs;
    /// -32600 when [`MAX_LISTENS`] listens are already open.
    pub fn open(&self, params: &Value, ws_id: &str, gate: &Gate) -> Result<Subscription, RpcError> {
        let uris = params
            .get("resourceSubscriptions")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("resourceSubscriptions must be an array of URIs"))?;
        if uris.is_empty() {
            return Err(bad("resourceSubscriptions names no URI"));
        }
        if uris.len() > MAX_URIS {
            return Err(bad(format!("a listen takes at most {MAX_URIS} URIs")));
        }
        let mut entries = BTreeMap::new();
        for u in uris {
            let u = u
                .as_str()
                .ok_or_else(|| bad("resourceSubscriptions must be an array of URIs"))?;
            let res = parse_uri(u, ws_id, gate)?;
            entries.entry(res).or_insert_with(|| u.to_owned());
        }
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_LISTENS).then_some(n + 1)
            })
            .map_err(|_| {
                RpcError::new(
                    INVALID_REQUEST,
                    format!("at most {MAX_LISTENS} listens may be open at once"),
                )
            })?;
        Ok(Subscription {
            entries,
            _slot: Slot(Arc::clone(&self.active)),
        })
    }

    /// Listens open now.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

/// The event stream of `Workspace.Watch`.
pub type Events = BoxStream<'static, Result<wire::WatchEvent, Status>>;

/// Opens `Workspace.Watch` for `ws_id` on the daemon channel, from now on.
///
/// # Errors
/// -32603 when the daemon refuses the stream.
pub async fn open_watch(channel: Channel, ws_id: &str) -> Result<Events, RpcError> {
    let s = WorkspaceClient::new(channel)
        .watch(wire::WatchRequest {
            ws_id: ws_id.to_owned(),
            since_token: String::new(),
        })
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("the watch stream failed: {}", e.message())))?;
    Ok(s.into_inner().boxed())
}

impl Subscription {
    /// The subscribed URIs `ev` updates, in a stable order.
    #[must_use]
    pub fn updates(&self, ev: &wire::WatchEvent) -> Vec<&str> {
        let kind = Kind::try_from(ev.kind).ok();
        let (status, smartlog, files, all) = match kind {
            Some(Kind::Paths) => (true, true, true, false),
            Some(Kind::Head) => (true, true, false, false),
            Some(Kind::Stack) => (false, true, false, false),
            Some(Kind::Resync) => (true, true, true, true),
            _ => return Vec::new(),
        };
        let changed: HashMap<&[u8], ()> = if files && !all {
            ev.paths.iter().map(|p| (p.as_ref(), ())).collect()
        } else {
            HashMap::new()
        };
        self.entries
            .iter()
            .filter(|(res, _)| match res {
                Resource::Status => status,
                Resource::Smartlog => smartlog,
                Resource::File(p) => all || (files && changed.contains_key(p.as_slice())),
            })
            .map(|(_, uri)| uri.as_str())
            .collect()
    }

    /// Forwards `events` as `notifications/resources/updated` until the stream ends or fails, or
    /// `out` closes. Dropping the future (the request was cancelled) ends the listen.
    ///
    /// # Errors
    /// -32603 when the stream breaks: the client opens a new listen.
    pub async fn run(self, mut events: Events, out: &Notifier) -> Result<(), RpcError> {
        while let Some(ev) = events.next().await {
            let ev = ev.map_err(|e| {
                RpcError::new(INTERNAL_ERROR, format!("the watch stream failed: {}", e.message()))
            })?;
            for uri in self.updates(&ev) {
                let n = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/updated",
                    "params": { "uri": uri },
                });
                if out.send(n).await.is_err() {
                    return Ok(());
                }
            }
        }
        Err(RpcError::new(
            INTERNAL_ERROR,
            "the watch stream ended; listen again",
        ))
    }
}

enum Line {
    Eof,
    TooLong,
    Complete,
}

/// Reads one line without holding more than `cap` bytes.
async fn read_line<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<Line> {
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(match (any, too_long) {
                (false, _) => Line::Eof,
                (true, true) => Line::TooLong,
                (true, false) => Line::Complete,
            });
        }
        any = true;
        let (take, found) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            let body = &chunk[..if found { take - 1 } else { take }];
            if buf.len() + body.len() > cap {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(body);
            }
        }
        r.consume(take);
        if found {
            return Ok(if too_long { Line::TooLong } else { Line::Complete });
        }
    }
}

fn error_line(id: &Value, e: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() })
}

/// The stdio loop of `asc mcp`: newline-delimited JSON-RPC on `reader` and `writer`. It is the
/// loop of `Handler::serve`, plus `subscriptions/listen`. Messages run concurrently; a closed
/// reader ends every listen and everything else that runs.
///
/// # Errors
/// An I/O error on the reader or writer.
pub async fn serve<B, R, W>(
    handler: Handler<B>,
    backend: LocalBackend,
    mut reader: R,
    mut writer: W,
    caller: Caller,
) -> io::Result<()>
where
    B: McpBackend,
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Value>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await?;
        }
        io::Result::Ok(())
    });
    let backend = Arc::new(backend);
    let ws_id = backend.ws_id().to_owned();
    let gate = backend.gate();
    let listens = Listens::default();
    let caller = Arc::new(caller);
    let mut running: HashMap<String, AbortHandle> = HashMap::new();
    let mut tasks: JoinSet<Option<String>> = JoinSet::new();
    let mut buf = Vec::new();
    let result: io::Result<()> = loop {
        buf.clear();
        // A finished listen leaves the table.
        while let Some(done) = tasks.try_join_next() {
            if let Ok(Some(key)) = done {
                running.remove(&key);
            }
        }
        match read_line(&mut reader, &mut buf, MAX_MESSAGE_BYTES).await {
            Err(e) => break Err(e),
            Ok(Line::Eof) => break Ok(()),
            Ok(Line::TooLong) => {
                let e = RpcError::new(INVALID_REQUEST, "message too large");
                let _ = tx.send(error_line(&Value::Null, &e)).await;
            }
            Ok(Line::Complete) => {
                if buf.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
                    let e = RpcError::new(asc_mcp::errors::PARSE_ERROR, "parse error");
                    let _ = tx.send(error_line(&Value::Null, &e)).await;
                    continue;
                };
                let method = msg.get("method").and_then(Value::as_str);
                let id = msg.get("id").filter(|i| i.is_string() || i.is_number());
                if method == Some("notifications/cancelled")
                    && id.is_none()
                    && let Some(rid) = msg.pointer("/params/requestId")
                    && let Some(h) = running.remove(&rid.to_string())
                {
                    h.abort();
                    continue;
                }
                if method == Some("subscriptions/listen")
                    && let Some(id) = id
                {
                    let id = id.clone();
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    let sub = listens.open(&params, &ws_id, &gate);
                    let key = id.to_string();
                    let (tx2, be, wid) = (tx.clone(), Arc::clone(&backend), ws_id.clone());
                    let events = match sub {
                        Ok(sub) => match be.watch_channel().await {
                            Ok(ch) => open_watch(ch, &wid).await.map(|e| (sub, e)),
                            Err(e) => Err(RpcError::new(INTERNAL_ERROR, e.message.clone())),
                        },
                        Err(e) => Err(e),
                    };
                    let h = tasks.spawn(async move {
                        let resp = match events {
                            Ok((sub, ev)) => match sub.run(ev, &tx2).await {
                                Ok(()) => return Some(id.to_string()),
                                Err(e) => error_line(&id, &e),
                            },
                            Err(e) => error_line(&id, &e),
                        };
                        let _ = tx2.send(resp).await;
                        Some(id.to_string())
                    });
                    running.insert(key, h);
                    continue;
                }
                // Everything else is the protocol layer's.
                let (h, tx2, caller) = (handler.clone(), tx.clone(), Arc::clone(&caller));
                tasks.spawn(async move {
                    if let Some(resp) = h.handle_message(msg, &caller, &tx2).await {
                        let _ = tx2.send(resp).await;
                    }
                    None
                });
            }
        }
    };
    // The stream is closed: listens end, and what still runs is cancelled with the handler.
    for h in running.values() {
        h.abort();
    }
    tasks.shutdown().await;
    drop(tx);
    writer_task.await.map_err(io::Error::other)??;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_wc::ignore::IgnoreSources;

    fn gate(root: &std::path::Path) -> Gate {
        let none = root.join("none");
        Gate::new(
            root.to_path_buf(),
            IgnoreSources {
                asc_excludes: none.clone(),
                core_excludes: None,
                local_exclude: none.clone(),
                common_exclude: none,
            },
        )
    }

    fn open(l: &Listens, uris: &[&str], g: &Gate) -> Result<Subscription, RpcError> {
        l.open(&json!({ "resourceSubscriptions": uris }), "w1", g)
    }

    fn ev(kind: Kind, paths: &[&str]) -> wire::WatchEvent {
        wire::WatchEvent {
            kind: kind as i32,
            paths: paths.iter().map(|p| p.as_bytes().to_vec().into()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn events_map_to_the_uris_they_change() {
        let d = tempfile::tempdir().unwrap();
        let g = gate(d.path());
        let s = open(
            &Listens::default(),
            &[
                "asc://ws/w1/status",
                "asc://ws/w1/smartlog",
                "asc://ws/w1/file/src/a.rs",
                "asc://ws/w1/file/src%2Fb.rs",
            ],
            &g,
        )
        .unwrap();
        let mut head = s.updates(&ev(Kind::Head, &[]));
        head.sort_unstable();
        assert_eq!(head, ["asc://ws/w1/smartlog", "asc://ws/w1/status"]);
        assert_eq!(s.updates(&ev(Kind::Stack, &[])), ["asc://ws/w1/smartlog"]);
        let p = s.updates(&ev(Kind::Paths, &["src/b.rs"]));
        assert!(p.contains(&"asc://ws/w1/file/src%2Fb.rs"));
        assert!(!p.contains(&"asc://ws/w1/file/src/a.rs"));
        assert_eq!(s.updates(&ev(Kind::Resync, &[])).len(), 4);
        assert!(s.updates(&ev(Kind::Stage, &["src/a.rs"])).is_empty());
    }

    #[test]
    fn at_most_two_listens_and_64_uris() {
        let d = tempfile::tempdir().unwrap();
        let g = gate(d.path());
        let l = Listens::default();
        let a = open(&l, &["asc://ws/w1/status"], &g).unwrap();
        let _b = open(&l, &["asc://ws/w1/status"], &g).unwrap();
        let e = open(&l, &["asc://ws/w1/status"], &g).unwrap_err();
        assert_eq!(e.code, INVALID_REQUEST);
        drop(a);
        assert_eq!(l.active(), 1);
        open(&l, &["asc://ws/w1/status"], &g).unwrap();
        let many: Vec<String> = (0..=MAX_URIS)
            .map(|i| format!("asc://ws/w1/file/f{i}.txt"))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let l2 = Listens::default();
        assert_eq!(open(&l2, &refs, &g).unwrap_err().code, INVALID_PARAMS);
        assert_eq!(l2.active(), 0);
        open(&l2, &refs[..MAX_URIS], &g).unwrap();
    }

    #[test]
    fn hidden_and_foreign_uris_are_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".ascignore"), "secret.txt\n").unwrap();
        let g = gate(d.path());
        let l = Listens::default();
        for uri in [
            "asc://ws/other/status",
            "asc://ws/w1/file/secret.txt",
            "asc://ws/w1/file/..%2Fx",
            "asc://ws/w1/file/%2Fetc%2Fpasswd",
            "asc://ws/w1/file/a/../b",
            "asc://ws/w1/file/",
            "asc://ws/w1/status?x=1",
            "asc://ws/w1/blob/HEAD/a",
            "asc://o/n/commit/HEAD",
            "https://example.com/",
            "asc://ws/w1/file/a%zz",
        ] {
            let e = open(&l, &[uri], &g).unwrap_err();
            assert_eq!(e.code, INVALID_PARAMS, "{uri}");
        }
        assert_eq!(l.active(), 0, "a refused listen holds no slot");
    }

    #[tokio::test]
    async fn run_sends_one_notification_per_updated_uri() {
        let d = tempfile::tempdir().unwrap();
        let g = gate(d.path());
        let s = open(&Listens::default(), &["asc://ws/w1/smartlog"], &g).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let events: Events = futures::stream::iter([
            Ok(ev(Kind::Stack, &[])),
            Ok(ev(Kind::Paths, &["x"])),
        ])
        .boxed();
        let r = s.run(events, &tx).await;
        assert!(r.is_err(), "an ended stream is reported so the client listens again");
        drop(tx);
        let mut n = 0;
        while let Some(m) = rx.recv().await {
            assert_eq!(m["method"], "notifications/resources/updated");
            assert_eq!(m["params"]["uri"], "asc://ws/w1/smartlog");
            n += 1;
        }
        assert_eq!(n, 2);
    }
}
