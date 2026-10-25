//! `LocalBackend`: the `McpBackend` of `asc mcp` (SPEC 22.7.2, unit M09).
//!
//! Routing:
//!
//! - `Workspace.*` tools go to the daemon over IPC. If no daemon runs, the first such call starts
//!   it, as the CLI does (SPEC 9.1), and mounts the workspace. Only when `ASC_NO_DAEMON` is set do
//!   they run in-process: the plugin service functions of the daemon are called directly (plain
//!   workspaces only), `state.db` is opened per call and never held, and its schema version is
//!   compared with the one seen at start. A mismatch answers `NOT_AVAILABLE` ("restart asc mcp")
//!   instead of racing a daemon or a migration.
//! - `show`, `read_file`, `ls_tree` and `grep` have no daemon RPC; they run the matching read verb
//!   as a child of this binary (stdin null, stdout piped, no daemon, no aliases, literal
//!   pathspecs), so the answer is what the CLI prints.
//! - `worktrees` lists only the worktrees of this repository (the daemon lists every mounted
//!   repository), from the same verb the daemon runs.
//! - Tools that need an asc server (`Repo`, `Changes`, `Projects`) answer `NOT_AVAILABLE`.
//!   TODO(U77): route them through `RpcBackend::with_server` once the resolved credential of the
//!   remote is exposed; today `ServerRemote` keeps it private.
//! - Writes are M10; none is served here.
//!
//! Every `path` and `paths` argument passes the [path gate](super::pathgate) first, and a
//! `worktree` argument is accepted only if it is one of this repository's worktrees.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::catalogue::{self, Needs, ToolDef};
use asc_mcp::cursor::Position;
use asc_mcp::errors::{ToolError, code};
use asc_mcp::rpc_backend::RpcBackend;
use asc_mcp::schema::Surface;
use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::json;
use asc_rpc::server::wire;
use asc_state::{AscHome, StateDb};
use asc_wc::ignore::IgnoreSources;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, MethodDescriptor};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tonic::Status;
use tonic::transport::Channel;

use super::pathgate::{Disk, Gate, Refusal};
use crate::cli::common::{CliError, Ctx, WsCtx};
use crate::daemon::lifecycle::{self, SystemProcess};
use crate::daemon::metrics::{EventLog, Metrics};
use crate::daemon::service::State;
use crate::daemon::{plugin_api, plugin_api_read, plugin_api_stack};

/// Arguments the MCP layer owns; they are never part of a wire request.
const PLUMBING: [&str; 5] = ["cursor", "max_tokens", "worktree", "repo", "ws_id"];
/// Most bytes a child's answer may hold.
const MAX_CHILD_OUT: usize = 16 * 1024 * 1024;
/// Most stderr bytes kept from a child.
const MAX_CHILD_ERR: usize = 16 * 1024;
/// How long one child may run.
const CHILD_LIMIT: Duration = Duration::from_secs(120);
/// Largest file `read_file` reads (SPEC 22.3.2: 256 KiB per call, from a file of at most this).
const MAX_FILE: usize = 32 * 1024 * 1024;
/// Bytes and lines per `read_file` call.
const READ_BYTES: usize = 256 * 1024;
const READ_LINES: u64 = 400;
/// Longest revision, pattern or other text argument.
const MAX_TEXT: usize = 4096;
/// Longest message text in an error.
const MAX_ERROR_TEXT: usize = 512;

/// One worktree of this repository.
struct Target {
    ws_id: String,
    gate: Gate,
}

struct DaemonLink {
    cfg: lifecycle::Config,
    proc: SystemProcess,
    channel: tokio::sync::OnceCell<Channel>,
    mounted: Mutex<HashSet<String>>,
}

struct InProc {
    home: AscHome,
    /// `meta.schema_version` seen at start.
    schema: i64,
}

enum Route {
    Daemon(Box<DaemonLink>),
    InProcess(InProc),
}

struct Inner {
    root: PathBuf,
    ws_id: String,
    sources: IgnoreSources,
    route: Route,
    pool: DescriptorPool,
}

/// The `McpBackend` for a workspace on this machine.
#[derive(Clone)]
pub struct LocalBackend {
    inner: Arc<Inner>,
}

impl LocalBackend {
    /// A backend for `ws`. The daemon is not started until a call needs it.
    ///
    /// # Errors
    /// The daemon's home or `state.db` cannot be read.
    pub async fn open(ctx: &Ctx, ws: &WsCtx) -> Result<Self, CliError> {
        let route = if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
            let schema = ws.db.schema_version().await?;
            Route::InProcess(InProc {
                home: ws.home.clone(),
                schema,
            })
        } else {
            let (cfg, proc) = crate::cli::daemon::system()?;
            Route::Daemon(Box::new(DaemonLink {
                cfg,
                proc,
                channel: tokio::sync::OnceCell::new(),
                mounted: Mutex::new(HashSet::new()),
            }))
        };
        let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET)
            .map_err(|e| CliError::Fatal(format!("asc mcp: descriptor set: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                root: ws.projection.toplevel().to_path_buf(),
                ws_id: ws.workspace.ws.id.to_string(),
                sources: crate::cli::check_ignore::ignore_sources(ctx, ws),
                route,
                pool,
            }),
        })
    }
}

// ------------------------------------------------------------------------------ small helpers

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_TEXT)
        .collect()
}

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::new("INVALID_ARGUMENT", msg)
}

fn map_status(s: &Status) -> ToolError {
    let message = clean(s.message());
    match s.code() {
        tonic::Code::NotFound | tonic::Code::PermissionDenied => {
            ToolError::new(code::NOT_FOUND, message)
        }
        tonic::Code::InvalidArgument | tonic::Code::OutOfRange => invalid(message),
        tonic::Code::Unavailable | tonic::Code::FailedPrecondition => {
            ToolError::not_available(message)
        }
        tonic::Code::Cancelled => ToolError::internal("the call was cancelled"),
        tonic::Code::DeadlineExceeded => ToolError::internal("the call timed out"),
        _ => ToolError::internal(message),
    }
}

fn refusal(r: &Refusal) -> ToolError {
    ToolError::new(code::PATH_POLICY, r.to_string()).with_paths(vec![r.path().to_owned()])
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(""),
        Some(Value::String(s)) => {
            if s.len() > MAX_TEXT || s.bytes().any(|b| b == 0) {
                Err(invalid(format!("{key} is too long or holds a NUL")))
            } else {
                Ok(s)
            }
        }
        Some(_) => Err(invalid(format!("{key} must be a string"))),
    }
}

/// A revision text that cannot be taken for an option or split into two arguments.
fn rev_arg<'a>(args: &'a Value, key: &str, default: &'a str) -> Result<&'a str, ToolError> {
    let r = text_arg(args, key)?;
    let r = if r.is_empty() { default } else { r };
    if r.starts_with('-') || r.bytes().any(|b| b == b'\n' || b == b'\r') {
        return Err(invalid(format!("{key} must be a revision, not an option")));
    }
    Ok(r)
}

fn uint(args: &Value, key: &str, default: u64, max: u64) -> u64 {
    args.get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .min(max)
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn messages(items: Vec<Value>, ctx: &CallContext) -> Reply {
    let skip = match &ctx.position {
        Some(Position::Index { index }) => usize::try_from(*index).unwrap_or(usize::MAX),
        _ => 0,
    };
    Reply::Messages(stream::iter(items.into_iter().skip(skip).map(Ok)).boxed())
}

// ------------------------------------------------------------------------------ child asc

struct Child {
    stdout: Vec<u8>,
    code: i32,
    over: bool,
}

async fn read_capped<R: AsyncRead + Unpin>(r: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.take(cap as u64 + 1).read_to_end(&mut buf).await?;
    let over = buf.len() > cap;
    buf.truncate(cap);
    Ok((buf, over))
}

impl Inner {
    /// Runs `asc <args>` in `root`: no daemon, no aliases, literal pathspecs, null stdin, piped
    /// output (so a hook or a verb that prints cannot reach the protocol channel). The child is
    /// killed when the call is cancelled or runs too long.
    async fn run_asc(
        &self,
        root: &Path,
        args: Vec<OsString>,
        cap: usize,
        ctx: Option<&CallContext>,
    ) -> Result<Child, ToolError> {
        let verb = args
            .first()
            .map(|a| a.to_string_lossy().into_owned())
            .unwrap_or_default();
        let exe = std::env::current_exe()
            .map_err(|e| ToolError::internal(format!("cannot find the asc binary: {e}")))?;
        let mut c = tokio::process::Command::new(exe);
        c.args(&args)
            .current_dir(root)
            .env("ASC_NO_DAEMON", "1")
            .env("ASC_NO_ALIASES", "1")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = c
            .spawn()
            .map_err(|e| ToolError::internal(format!("cannot run asc {verb}: {e}")))?;
        let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::internal("the child has no pipes"));
        };
        let work = async { tokio::join!(read_capped(out, cap), read_capped(err, MAX_CHILD_ERR)) };
        let cancelled = async {
            match ctx {
                Some(c) => c.cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let finished = tokio::select! {
            r = tokio::time::timeout(CHILD_LIMIT, work) => r.ok(),
            () = cancelled => None,
        };
        let Some((o, e)) = finished else {
            let _ = child.kill().await;
            return Err(ToolError::internal(format!(
                "asc {verb} was cancelled or ran longer than {} s",
                CHILD_LIMIT.as_secs()
            )));
        };
        let ((stdout, over), (stderr, _)) = (
            o.map_err(|e| ToolError::internal(e.to_string()))?,
            e.map_err(|e| ToolError::internal(e.to_string()))?,
        );
        if over {
            let _ = child.kill().await;
        }
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .map_err(|_| ToolError::internal(format!("asc {verb} did not end")))?
            .map_err(|e| ToolError::internal(e.to_string()))?;
        let code = status.code().unwrap_or(-1);
        if code != 0 && !stderr.is_empty() {
            // Child stderr goes to our stderr only, bounded and free of control characters.
            eprintln!(
                "asc mcp: asc {verb}: {}",
                clean(&String::from_utf8_lossy(&stderr))
            );
        }
        Ok(Child { stdout, code, over })
    }

    async fn run_ok(
        &self,
        root: &Path,
        args: Vec<OsString>,
        cap: usize,
        ctx: &CallContext,
        what: &str,
    ) -> Result<Vec<u8>, ToolError> {
        let r = self.run_asc(root, args, cap, Some(ctx)).await?;
        if r.over {
            return Err(ToolError::budget(format!(
                "the answer of {what} is too large; narrow the request"
            )));
        }
        if r.code != 0 {
            return Err(ToolError::new(
                code::NOT_FOUND,
                format!(
                    "{what} failed (exit {}); check the revision and path",
                    r.code
                ),
            ));
        }
        Ok(r.stdout)
    }

    /// This repository's worktrees: `(path, ws_id)`.
    async fn worktree_list(&self) -> Result<Vec<(PathBuf, String)>, ToolError> {
        let out = self
            .run_asc(
                &self.root,
                vec!["ws-api".into(), "worktrees".into()],
                MAX_CHILD_OUT,
                None,
            )
            .await?;
        if out.code != 0 {
            return Err(ToolError::not_available("the worktrees cannot be listed"));
        }
        let line = out
            .stdout
            .split(|b| *b == b'\n')
            .find(|l| !l.trim_ascii().is_empty())
            .unwrap_or_default();
        let doc: Value = serde_json::from_slice(line)
            .map_err(|_| ToolError::internal("asc ws-api worktrees printed bad JSON"))?;
        Ok(doc
            .get("worktrees")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|w| {
                        Some((
                            PathBuf::from(w.get("path")?.as_str()?),
                            w.get("ws_id")?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn target(&self, args: &Value) -> Result<Target, ToolError> {
        let Some(wanted) = args.get("worktree").and_then(Value::as_str) else {
            return Ok(Target {
                ws_id: self.ws_id.clone(),
                gate: Gate::new(self.root.clone(), self.sources.clone()),
            });
        };
        let wanted_canon = std::fs::canonicalize(wanted)
            .map_err(|_| invalid("worktree is not a worktree of this workspace"))?;
        for (path, ws_id) in self.worktree_list().await? {
            if std::fs::canonicalize(&path).is_ok_and(|p| p == wanted_canon) {
                return Ok(Target {
                    ws_id,
                    gate: Gate::new(path, self.sources.clone()),
                });
            }
        }
        Err(invalid("worktree is not a worktree of this workspace"))
    }
}

// ------------------------------------------------------------------------------ the path gate

/// Whether the call reads a revision (so the working copy's links do not matter).
fn disk_for(tool: &str, args: &Value) -> Disk {
    let has_rev = args
        .get("rev")
        .and_then(Value::as_str)
        .is_some_and(|r| !r.is_empty());
    match tool {
        "ls_tree" => Disk::Revision,
        "read_file" | "blame" | "grep" if has_rev => Disk::Revision,
        _ => Disk::Worktree,
    }
}

fn check_paths(gate: &Gate, tool: &str, args: &Value) -> Result<(), ToolError> {
    let disk = disk_for(tool, args);
    let dir_hint = tool == "ls_tree";
    if let Some(p) = args.get("path") {
        let p = p.as_str().ok_or_else(|| invalid("path must be a string"))?;
        gate.check(p, disk, dir_hint).map_err(|r| refusal(&r))?;
    }
    if let Some(ps) = args.get("paths") {
        let ps = ps
            .as_array()
            .ok_or_else(|| invalid("paths must be an array"))?;
        if ps.len() > 1024 {
            return Err(invalid("too many paths"));
        }
        for p in ps {
            let p = p.as_str().ok_or_else(|| invalid("paths must be strings"))?;
            gate.check(p, disk, false).map_err(|r| refusal(&r))?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ local verbs

fn os(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `+0130` / `-0500` as minutes east of UTC.
fn tz_minutes(tz: &str) -> i64 {
    let (sign, digits) = match tz.as_bytes().first() {
        Some(b'-') => (-1, &tz[1..]),
        Some(b'+') => (1, &tz[1..]),
        _ => (1, tz),
    };
    let n: i64 = digits.parse().unwrap_or(0);
    sign * ((n / 100) * 60 + n % 100)
}

/// `"1792900442 +0200"` as a `Signature`.
fn signature(name: &str, email: &str, date: &str) -> Value {
    let (secs, tz) = date.split_once(' ').unwrap_or((date, "+0000"));
    json!({ "name": name, "email": email, "time": secs.parse::<i64>().unwrap_or(0), "tz_min": tz_minutes(tz) })
}

impl Inner {
    async fn show(&self, t: &Target, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let rev = rev_arg(args, "rev", "HEAD")?;
        let max_changes = usize::try_from(uint(args, "max_changes", 300, 300)).unwrap_or(300);
        let format = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd%x1f%B";
        let mut a = os(&["show", "-s", format, "--date=raw"]);
        a.push(rev.into());
        let head = self
            .run_ok(t.gate.root(), a, 1024 * 1024, ctx, "asc show")
            .await?;
        let head = lossy(&head);
        let f: Vec<&str> = head.splitn(9, '\x1f').collect();
        if f.len() != 9 {
            return Err(ToolError::internal("asc show printed an unexpected header"));
        }
        let mut a = os(&[
            "diff-tree",
            "-r",
            "-z",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-M",
        ]);
        a.push(rev.into());
        let raw = self
            .run_ok(t.gate.root(), a, MAX_CHILD_OUT, ctx, "asc diff-tree")
            .await?;
        let mut changes = Vec::new();
        let mut it = raw.split(|b| *b == 0).filter(|s| !s.is_empty());
        let mut total = 0usize;
        while let Some(st) = it.next() {
            let status = lossy(st);
            let first = status.chars().next().unwrap_or('M');
            let path = lossy(it.next().unwrap_or_default());
            let old = matches!(first, 'R' | 'C').then(|| path.clone());
            let path = if old.is_some() {
                lossy(it.next().unwrap_or_default())
            } else {
                path
            };
            total += 1;
            if changes.len() < max_changes {
                let mut c = json!({ "path": path, "status": first.to_string() });
                if let Some(old) = old {
                    c["old_path"] = Value::String(old);
                }
                changes.push(c);
            }
        }
        let parents: Vec<Value> = f[1]
            .split_whitespace()
            .map(|p| Value::String(p.to_owned()))
            .collect();
        let mut out = json!({
            "header": {
                "id": f[0],
                "git": f[0],
                "parents": parents,
                "author": signature(f[2], f[3], f[4]),
                "committer": signature(f[5], f[6], f[7]),
                "message": f[8].trim_end_matches('\n'),
            },
            "changes": changes,
        });
        if total > max_changes {
            out["truncated"] = Value::Bool(true);
        }
        Ok(Reply::Messages(stream::iter([Ok(out)]).boxed()))
    }

    async fn ls_tree(
        &self,
        t: &Target,
        args: &Value,
        ctx: &CallContext,
    ) -> Result<Reply, ToolError> {
        let rev = rev_arg(args, "rev", "HEAD")?;
        let path = text_arg(args, "path")?;
        let recursive = flag(args, "recursive");
        let limit = usize::try_from(uint(args, "limit", 200, 1000)).unwrap_or(200);
        let after = text_arg(args, "after")?;
        let parts = super::pathgate::components(path).map_err(|r| refusal(&r))?;
        if recursive && parts.is_empty() {
            return Err(invalid(
                "recursive needs a non-root path; give a subdirectory or use grep",
            ));
        }
        let mut a = os(&["ls-tree", "-z", "-l"]);
        if recursive {
            a.push("-r".into());
        }
        a.push(rev.into());
        if !parts.is_empty() {
            a.push(format!("{}/", parts.join("/")).into());
        }
        let raw = self
            .run_ok(t.gate.root(), a, MAX_CHILD_OUT, ctx, "asc ls-tree")
            .await?;
        let mut items = Vec::new();
        let mut more = false;
        for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let (meta, name) = (lossy(&rec[..tab]), lossy(&rec[tab + 1..]));
            let mut m = meta.split_whitespace();
            let (Some(mode), Some(_kind), Some(id), Some(size)) =
                (m.next(), m.next(), m.next(), m.next())
            else {
                continue;
            };
            if !after.is_empty() && name.as_str() <= after {
                continue;
            }
            if items.len() >= limit {
                more = true;
                break;
            }
            let base = name.rsplit('/').next().unwrap_or(&name).to_owned();
            items.push(json!({ "entries": [{
                "name": base,
                "path": name,
                "mode": u32::from_str_radix(mode, 8).unwrap_or(0),
                "git": id,
                "size": size.parse::<u64>().unwrap_or(0),
            }] }));
        }
        if more {
            items.push(json!({ "truncated": true }));
        }
        Ok(messages(items, ctx))
    }

    async fn grep(&self, t: &Target, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let rev = rev_arg(args, "rev", "")?;
        let pattern = text_arg(args, "pattern")?;
        if pattern.is_empty() {
            return Err(invalid("pattern is required"));
        }
        let max = usize::try_from(uint(args, "max_results", 100, 1000)).unwrap_or(100);
        let mut a = os(&["grep", "-n"]);
        if flag(args, "fixed") {
            a.push("-F".into());
        }
        if flag(args, "ignore_case") {
            a.push("-i".into());
        }
        a.push("-e".into());
        a.push(pattern.into());
        if !rev.is_empty() {
            a.push(rev.into());
        }
        a.push("--".into());
        if let Some(ps) = args.get("paths").and_then(Value::as_array) {
            for p in ps.iter().filter_map(Value::as_str) {
                a.push(p.into());
            }
        }
        let r = self
            .run_asc(t.gate.root(), a, 8 * 1024 * 1024, Some(ctx))
            .await?;
        if r.code > 1 {
            return Err(invalid(
                "the search failed; check the pattern, revision and paths",
            ));
        }
        let text = lossy(&r.stdout);
        let prefix = if rev.is_empty() {
            String::new()
        } else {
            format!("{rev}:")
        };
        let mut items = Vec::new();
        let mut truncated = r.over;
        for line in text.lines() {
            let Some(rest) = line.strip_prefix(prefix.as_str()) else {
                continue;
            };
            let Some((path, number, body)) = split_match(rest) else {
                continue;
            };
            // Ignored files are never searched, whatever the revision holds.
            if !t.gate.shown_ok(path) {
                continue;
            }
            if items.len() >= max {
                truncated = true;
                break;
            }
            items.push(json!({ "matches": [{ "path": path, "line": number, "text": body }] }));
        }
        if truncated {
            items.push(json!({ "truncated": true }));
        }
        Ok(messages(items, ctx))
    }

    async fn read_file(
        &self,
        t: &Target,
        args: &Value,
        ctx: &CallContext,
    ) -> Result<Reply, ToolError> {
        let path = text_arg(args, "path")?;
        let rev = rev_arg(args, "rev", "")?;
        if rev.contains(':') {
            return Err(invalid(
                "rev must be a revision; the path is a separate argument",
            ));
        }
        let (content, total) = if rev.is_empty() {
            let abs = t
                .gate
                .check(path, Disk::Worktree, false)
                .map_err(|r| refusal(&r))?;
            let read = tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<u8>, u64)> {
                use std::io::Read as _;
                let f = Gate::open(&abs)?;
                let len = f.metadata()?.len();
                let mut buf = Vec::new();
                f.take(MAX_FILE as u64 + 1).read_to_end(&mut buf)?;
                Ok((buf, len))
            })
            .await
            .map_err(|e| ToolError::internal(e.to_string()))?;
            match read {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolError::new(code::NOT_FOUND, "no such file"));
                }
                // A link, a directory or a device that slipped past the check.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                    return Err(ToolError::new(
                        code::PATH_POLICY,
                        "not a regular file; links are never followed",
                    )
                    .with_paths(vec![clean(path)]));
                }
                Err(_) => return Err(ToolError::new(code::NOT_FOUND, "the file cannot be read")),
            }
        } else {
            t.gate
                .check(path, Disk::Revision, false)
                .map_err(|r| refusal(&r))?;
            let a = vec![
                OsString::from("cat-file"),
                "blob".into(),
                format!("{rev}:{path}").into(),
            ];
            let r = self.run_asc(t.gate.root(), a, MAX_FILE, Some(ctx)).await?;
            if r.code != 0 {
                return Err(ToolError::new(
                    code::NOT_FOUND,
                    "no such file at that revision",
                ));
            }
            let len = r.stdout.len() as u64;
            (r.stdout, len)
        };
        if content.len() > MAX_FILE {
            return Err(ToolError::budget(
                "the file is over 32 MiB; use grep or a revision range",
            ));
        }
        let binary = content.iter().take(8000).any(|b| *b == 0);
        if binary {
            let out = json!({ "total_size": total, "binary": true });
            return Ok(Reply::Messages(stream::iter([Ok(out)]).boxed()));
        }
        Ok(Reply::Messages(
            stream::iter([Ok(page(&content, total, args))]).boxed(),
        ))
    }

    async fn worktrees(&self, args: &Value) -> Result<Reply, ToolError> {
        let limit = usize::try_from(uint(args, "limit", 100, 100)).unwrap_or(100);
        let mut list = self.worktree_list_full().await?;
        let more = list.len() > limit;
        list.truncate(limit);
        let mut out = json!({ "worktrees": list });
        if more {
            out["truncated"] = Value::Bool(true);
        }
        Ok(Reply::Messages(stream::iter([Ok(out)]).boxed()))
    }

    async fn worktree_list_full(&self) -> Result<Vec<Value>, ToolError> {
        let out = self
            .run_asc(
                &self.root,
                vec!["ws-api".into(), "worktrees".into()],
                MAX_CHILD_OUT,
                None,
            )
            .await?;
        if out.code != 0 {
            return Err(ToolError::not_available("the worktrees cannot be listed"));
        }
        let line = out
            .stdout
            .split(|b| *b == b'\n')
            .find(|l| !l.trim_ascii().is_empty())
            .unwrap_or_default();
        let doc: Value = serde_json::from_slice(line)
            .map_err(|_| ToolError::internal("asc ws-api worktrees printed bad JSON"))?;
        Ok(doc
            .get("worktrees")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

/// `path:line:text` from `asc grep -n`: the first `:<digits>:` after at least one path byte.
fn split_match(s: &str) -> Option<(&str, u64, &str)> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        if b[i] == b':' {
            let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && b.get(i + 1 + digits) == Some(&b':') {
                let n = s[i + 1..i + 1 + digits].parse().ok()?;
                return Some((&s[..i], n, &s[i + 2 + digits..]));
            }
        }
        i += 1;
    }
    None
}

fn count_lines(content: &[u8]) -> u64 {
    let nl = content.iter().filter(|b| **b == b'\n').count() as u64;
    nl + u64::from(content.last().is_some_and(|b| *b != b'\n'))
}

/// One `read_file` page: lines `start_line..=end_line` (default from line 1) or, with `offset` and
/// no line range, bytes. At most 400 lines and 256 KiB; the reply says where to continue.
fn page(content: &[u8], total: u64, args: &Value) -> Value {
    let total_lines = count_lines(content);
    let byte_mode = args.get("offset").is_some()
        && args.get("start_line").is_none()
        && args.get("end_line").is_none();
    if byte_mode {
        let offset = usize::try_from(uint(args, "offset", 0, u64::MAX))
            .unwrap_or(usize::MAX)
            .min(content.len());
        let length = usize::try_from(uint(args, "length", READ_BYTES as u64, READ_BYTES as u64))
            .unwrap_or(READ_BYTES);
        let mut end = (offset + length).min(content.len());
        while end > offset && end < content.len() && (content[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        let mut start = offset;
        while start < end && (content[start] & 0xC0) == 0x80 {
            start += 1;
        }
        return json!({
            "data": lossy(&content[start..end]),
            "total_size": total,
            "total_lines": total_lines,
            "truncated": end < content.len(),
        });
    }
    let want_start = uint(args, "start_line", 1, u64::MAX).max(1);
    let want_end = args
        .get("end_line")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let mut text = String::new();
    let (mut n, mut last, mut taken, mut cut) = (1u64, 0u64, 0u64, false);
    for l in content.split_inclusive(|b| *b == b'\n') {
        if n > want_end {
            break;
        }
        if n >= want_start {
            if taken >= READ_LINES || (taken > 0 && text.len() + l.len() > READ_BYTES) {
                cut = true;
                break;
            }
            if l.len() > READ_BYTES {
                // One line alone over the byte budget: keep a prefix; the next call continues after it.
                let mut e = READ_BYTES;
                while e > 0 && (l[e] & 0xC0) == 0x80 {
                    e -= 1;
                }
                text.push_str(&String::from_utf8_lossy(&l[..e]));
                cut = true;
            } else {
                text.push_str(&String::from_utf8_lossy(l));
            }
            last = n;
            taken += 1;
            if cut {
                break;
            }
        }
        n += 1;
    }
    json!({
        "data": text,
        "total_size": total,
        "start_line": want_start,
        "end_line": if taken == 0 { want_start - 1 } else { last },
        "total_lines": total_lines,
        "truncated": cut,
    })
}

// ------------------------------------------------------------------------------ Workspace.*

fn workspace_method(rpc: &str) -> Option<&str> {
    rpc.split(" / ")
        .map(str::trim)
        .find_map(|alt| alt.strip_prefix("Workspace."))
}

impl DaemonLink {
    async fn channel(&self) -> Result<Channel, ToolError> {
        self.channel
            .get_or_try_init(|| async {
                lifecycle::ensure_running(&self.cfg, &self.proc)
                    .await
                    .map_err(|e| {
                        ToolError::not_available(format!(
                            "the daemon cannot be started: {}",
                            clean(&e.to_string())
                        ))
                    })?;
                Ok::<_, ToolError>(connect(&self.cfg.endpoint))
            })
            .await
            .cloned()
    }

    async fn mount(&self, ws_id: &str) -> Result<Channel, ToolError> {
        let ch = self.channel().await?;
        if self
            .mounted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(ws_id)
        {
            return Ok(ch);
        }
        WorkspaceClient::new(ch.clone())
            .mount(wire::MountRequest {
                ws_id: ws_id.to_owned(),
            })
            .await
            .map_err(|s| map_status(&s))?;
        self.mounted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ws_id.to_owned());
        Ok(ch)
    }

    /// The daemon went away: start it again and forget what was mounted.
    async fn restart(&self) -> Result<(), ToolError> {
        self.mounted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        lifecycle::ensure_running(&self.cfg, &self.proc)
            .await
            .map(|_| ())
            .map_err(|e| {
                ToolError::not_available(format!(
                    "the daemon cannot be started: {}",
                    clean(&e.to_string())
                ))
            })
    }
}

fn to_wire<T: prost_reflect::prost::Message + Default>(m: &DynamicMessage) -> Result<T, ToolError> {
    m.transcode_to::<T>()
        .map_err(|e| invalid(format!("request: {e}")))
}

fn to_dyn<T: prost_reflect::prost::Message>(
    desc: &MessageDescriptor,
    t: &T,
) -> Result<DynamicMessage, Status> {
    let mut d = DynamicMessage::new(desc.clone());
    d.transcode_from(t)
        .map_err(|e| Status::internal(format!("response: {e}")))?;
    Ok(d)
}

type DynStream = BoxStream<'static, Result<DynamicMessage, Status>>;

fn one<T: prost_reflect::prost::Message + Send + 'static>(
    desc: MessageDescriptor,
    r: Result<T, Status>,
) -> DynStream {
    stream::once(async move { r.and_then(|t| to_dyn(&desc, &t)) }).boxed()
}

fn many<T: prost_reflect::prost::Message + Send + 'static>(
    desc: MessageDescriptor,
    r: Result<plugin_api::PluginStream<T>, Status>,
) -> DynStream {
    match r {
        Ok(s) => s.map(move |t| t.and_then(|t| to_dyn(&desc, &t))).boxed(),
        Err(e) => stream::once(async move { Err(e) }).boxed(),
    }
}

impl Inner {
    fn method(&self, name: &str) -> Result<MethodDescriptor, ToolError> {
        self.pool
            .get_service_by_name("asc.v1.Workspace")
            .and_then(|s| s.methods().find(|m| m.name() == name))
            .ok_or_else(|| ToolError::not_available(format!("Workspace.{name} is not served")))
    }

    /// The wire request of `def` from the validated arguments, with `ws_id` set.
    fn build(
        &self,
        method: &MethodDescriptor,
        args: &Value,
        ctx: &CallContext,
        ws_id: &str,
    ) -> Result<DynamicMessage, ToolError> {
        let input = method.input();
        let mut obj: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        for k in PLUMBING {
            obj.remove(k);
        }
        // A `query` wrapper takes the flat arguments its inner message knows.
        if let Some(q) = input
            .get_field_by_name("query")
            .and_then(|f| f.kind().as_message().cloned())
            && !obj.contains_key("query")
        {
            let mut inner = Map::new();
            for k in obj.keys().cloned().collect::<Vec<_>>() {
                if input.get_field_by_name(&k).is_none()
                    && q.get_field_by_name(&k).is_some()
                    && let Some(v) = obj.remove(&k)
                {
                    inner.insert(k, v);
                }
            }
            obj.insert("query".into(), Value::Object(inner));
        }
        obj.retain(|k, _| input.get_field_by_name(k).is_some());
        if let Some(Position::Page { token }) = &ctx.position {
            for name in ["page_token", "after"] {
                if input.get_field_by_name(name).is_some() {
                    obj.insert(name.into(), Value::String(token.clone()));
                    break;
                }
            }
        }
        let mut msg = json::decode(&input, &Value::Object(obj))
            .map_err(|e| invalid(format!("{}: {e}", method.name())))?;
        if input.get_field_by_name("ws_id").is_some() {
            msg.set_field_by_name("ws_id", prost_reflect::Value::String(ws_id.to_owned()));
        }
        Ok(msg)
    }

    /// Opens `state.db` for one call. It is dropped with the call's stream.
    async fn state(&self, ip: &InProc, ws_id: &str) -> Result<Arc<State>, ToolError> {
        let restart = |why: String| ToolError::not_available(format!("{why}; restart asc mcp"));
        let db = StateDb::open(&ip.home)
            .await
            .map_err(|e| restart(format!("state.db: {}", clean(&e.to_string()))))?;
        let found = db
            .schema_version()
            .await
            .map_err(|e| restart(format!("state.db: {}", clean(&e.to_string()))))?;
        if found != ip.schema {
            return Err(restart(format!(
                "state.db schema changed from {} to {found}",
                ip.schema
            )));
        }
        let state = State::new(
            db,
            ip.home.clone(),
            None,
            Arc::new(Metrics::new()),
            Arc::new(EventLog::none()),
        );
        let id = ws_id
            .parse()
            .map_err(|_| ToolError::internal("malformed workspace id"))?;
        state.mount(id).await.map_err(|s| map_status(&s))?;
        Ok(state)
    }

    async fn in_process(
        &self,
        ip: &InProc,
        method: &MethodDescriptor,
        args: &Value,
        ctx: &CallContext,
        t: &Target,
    ) -> Result<DynStream, ToolError> {
        let msg = self.build(method, args, ctx, &t.ws_id)?;
        let state = self.state(ip, &t.ws_id).await?;
        let out = method.output();
        let s: DynStream = match method.name() {
            "Status" => one(out, plugin_api::status(&state, to_wire(&msg)?).await),
            "Diff" => many(out, plugin_api::diff(&state, to_wire(&msg)?).await),
            "Log" => many(out, plugin_api::log(&state, to_wire(&msg)?).await),
            "Blame" => many(out, plugin_api::blame(&state, to_wire(&msg)?).await),
            "Affected" => one(out, plugin_api::affected(&state, to_wire(&msg)?).await),
            "Stacks" => one(out, plugin_api_stack::stacks(&state, to_wire(&msg)?).await),
            "Conflicts" => one(
                out,
                plugin_api_stack::conflicts(&state, to_wire(&msg)?).await,
            ),
            "ReadFile" => many(
                out,
                plugin_api_read::read_file(&state, to_wire(&msg)?).await,
            ),
            other => {
                return Err(ToolError::not_available(format!(
                    "Workspace.{other} is not served by asc mcp"
                )));
            }
        };
        // The state lives as long as the stream is read.
        Ok(s.map(move |x| {
            let _keep = &state;
            x
        })
        .boxed())
    }

    async fn daemon(
        &self,
        link: &DaemonLink,
        tool: &str,
        args: &Value,
        ctx: &CallContext,
        t: &Target,
    ) -> Result<Reply, ToolError> {
        let mut attempt = 0;
        loop {
            let r = match link.mount(&t.ws_id).await {
                Ok(channel) => {
                    RpcBackend::new(Surface::Stdio)
                        .with_workspace(channel, &t.ws_id)
                        .call(tool, args, ctx)
                        .await
                }
                Err(e) => Err(e),
            };
            match r {
                Err(e) if e.code == code::NOT_AVAILABLE && attempt == 0 => {
                    attempt += 1;
                    link.restart().await?;
                }
                other => return other,
            }
        }
    }

    async fn workspace(
        &self,
        def: &ToolDef,
        tool: &str,
        args: &Value,
        ctx: &CallContext,
        t: &Target,
    ) -> Result<Reply, ToolError> {
        let Some(name) = workspace_method(def.rpc) else {
            return Err(ToolError::not_available(format!(
                "{tool} needs an asc server, and none is configured for this workspace"
            )));
        };
        match &self.route {
            Route::Daemon(link) => self.daemon(link, tool, args, ctx, t).await,
            Route::InProcess(ip) => {
                let method = self.method(name)?;
                let raw = self.in_process(ip, &method, args, ctx, t).await?;
                let skip = match &ctx.position {
                    Some(Position::Index { index }) => {
                        usize::try_from(*index).unwrap_or(usize::MAX)
                    }
                    _ => 0,
                };
                let budget = ctx.budget_bytes;
                let items = raw
                    .skip(skip)
                    .map(|r| {
                        r.map_err(|s| map_status(&s)).and_then(|m| {
                            json::encode(&m).map_err(|e| {
                                ToolError::internal(format!("response cannot be shown: {e}"))
                            })
                        })
                    })
                    .scan((0u64, false), move |(used, done), item| {
                        if *done {
                            return futures::future::ready(None);
                        }
                        if let Ok(v) = &item {
                            *used = used.saturating_add(v.to_string().len() as u64);
                            *done = *used > budget;
                        }
                        futures::future::ready(Some(item))
                    })
                    .take_until(ctx.cancel.clone().cancelled_owned())
                    .boxed();
                Ok(Reply::Messages(items))
            }
        }
    }
}

#[async_trait::async_trait]
impl McpBackend for LocalBackend {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let inner = &*self.inner;
        let def = catalogue::find(tool, Surface::Stdio)
            .ok_or_else(|| ToolError::not_available(format!("{tool} is not served here")))?;
        // TODO(M10): the write tools run as child verbs.
        if !def.ann.read_only || def.needs != Needs::None {
            return Err(ToolError::not_available(format!(
                "{tool} is not served by this build"
            )));
        }
        let target = inner.target(args).await?;
        check_paths(&target.gate, tool, args)?;
        match tool {
            "show" => inner.show(&target, args, ctx).await,
            "ls_tree" => inner.ls_tree(&target, args, ctx).await,
            "grep" => inner.grep(&target, args, ctx).await,
            "read_file" => inner.read_file(&target, args, ctx).await,
            "worktrees" => inner.worktrees(args).await,
            _ => inner.workspace(def, tool, args, ctx, &target).await,
        }
    }

    async fn worktrees(&self) -> Result<Vec<PathBuf>, ToolError> {
        Ok(self
            .inner
            .worktree_list()
            .await?
            .into_iter()
            .map(|(p, _)| p)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_lines_split_at_the_first_number() {
        assert_eq!(split_match("f.txt:3:hello"), Some(("f.txt", 3, "hello")));
        assert_eq!(
            split_match("a:b/c.txt:12:x:7:y"),
            Some(("a:b/c.txt", 12, "x:7:y"))
        );
        assert_eq!(split_match("nonumber"), None);
    }

    #[test]
    fn timezones() {
        assert_eq!(tz_minutes("+0130"), 90);
        assert_eq!(tz_minutes("-0500"), -300);
        assert_eq!(tz_minutes("+0000"), 0);
    }

    #[test]
    fn line_counting() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a\nb\n"), 2);
        assert_eq!(count_lines(b"a\nb"), 2);
    }
}
