//! The plugin API of the daemon (SPEC §7 `workspace.proto`, §11.2; U73): `Status`, `Diff`,
//! `Log`, `Blame`, `Affected`, `Stage` and `Watch`, the stable RPCs ASC's IDE plugins use.
//!
//! **One engine path.** Each query RPC runs the matching `asc … --json` command in the mounted
//! workspace and decodes its JSON lines (the asc JSON profile) into the typed response, so an RPC
//! answers exactly what the CLI prints and the two cannot drift. The child is this very binary,
//! started with no daemon (`ASC_NO_DAEMON`), no aliases and literal pathspecs. `Stage` runs
//! `asc add`, `asc restore --staged` and `asc apply --cached` the same way, serialised with the
//! workspace's other work-tree changes.
//!
//! **Authorisation and bounds.** The transport admits only the current user, and every RPC acts
//! only on a workspace the daemon has mounted (`Mount` checked the registration and the root).
//! The root is checked again before every child is started, so a root swapped for a link is
//! refused. Revision text, paths, counts, argument bytes, patch size, output size, line size, run
//! time and concurrent watchers are all bounded; a revision never starts with `-`, and paths come
//! after `--` and are literal.
//!
//! **Watch.** A `Watch` stream is built from state the daemon keeps anyway, so it can resume
//! from a token without loss: the workspace journal (path hints), the `ref_log` (refs, HEAD), the
//! `op_log` (operations), the workspace row (HEAD and switches) and the staging rows. A token is
//! `v1.<epoch>.<journal seq>.<ref_log seq>.<op_log seq>.<head print>.<stage print>`; every event
//! carries the token that resumes right after it. A plain workspace has no provider to journal
//! edits, so while someone watches it the daemon scans the work tree (metadata only, ignore
//! rules applied, links never followed) and records what changed into the journal.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, UNIX_EPOCH};

use bytes::Bytes;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RepoPath;
use asc_rpc::server::wire;
use asc_state::WorkspaceId;
use asc_state::oplog;
use asc_state::state::git_map;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces::{self, Mode};
use asc_store::refs::RepoKey;
use asc_wc::journal::{Op, Token};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader,
};
use tonic::Status;

use super::service::{Mounted, State, parse_id, parse_paths};

/// A server stream of the plugin API.
pub type PluginStream<T> = BoxStream<'static, Result<T, Status>>;

/// Longest revision, ref or author text accepted from a client.
pub(super) const MAX_TEXT: usize = 1024;
/// Most revisions, selectors or ecosystems in one request.
const MAX_ITEMS: usize = 64;
/// Most bytes of path arguments on one child command line (Windows allows 32 KiB in all).
const MAX_ARG_BYTES: usize = 16 * 1024;
/// Largest `Stage.patch`.
const MAX_PATCH: usize = 8 * 1024 * 1024;
/// Largest answer of a one-message RPC.
pub(super) const MAX_OUT: usize = 16 * 1024 * 1024;
/// Largest line of a streamed answer.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Most bytes one stream carries.
const MAX_STREAM: usize = 256 * 1024 * 1024;
/// Most stderr bytes kept for an error message.
const MAX_ERR: usize = 64 * 1024;
/// How long one child may run.
const RUN_LIMIT: Duration = Duration::from_secs(300);
/// `Log` limit when the request names none, and the most it may name.
const DEFAULT_LOG: u32 = 1_000;
const MAX_LOG: u32 = 100_000;
/// Largest `-U` context.
const MAX_CONTEXT: u32 = 1_000;
/// Most concurrent `Watch` streams in the daemon.
const MAX_WATCHERS: usize = 128;
/// Longest accepted watch token.
const MAX_TOKEN: usize = 128;
/// Most unique paths in one `KIND_PATHS` event; more is a `KIND_RESYNC`.
const MAX_EVENT_PATHS: usize = 5_000;
/// Most `ref_log` rows read per poll.
const MAX_REF_ROWS: i64 = 500;
/// Most `op_log` rows read per poll.
const MAX_OP_ROWS: u32 = 100;
/// Most files a plain-workspace scan keeps; a larger tree is reported as a resync.
const MAX_SCAN_ENTRIES: usize = 200_000;
/// Most staged rows hashed for the staging print.
const MAX_STAGE_ROWS: usize = 200_000;
/// How often a `Watch` stream looks at the daemon's state.
const POLL: Duration = Duration::from_millis(200);
/// Scan interval bounds.
const SCAN_MIN: Duration = Duration::from_millis(250);
const SCAN_MAX: Duration = Duration::from_secs(5);
/// How long `Watch` waits for the first scan of a plain workspace.
const SCAN_WAIT: Duration = Duration::from_secs(30);
/// Consecutive poll failures after which a `Watch` stream ends.
const MAX_POLL_FAILURES: u32 = 25;

pub(super) fn bad(text: impl Into<String>) -> Status {
    Status::invalid_argument(text.into())
}

pub(super) fn internal(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}

fn unavailable(e: impl std::fmt::Display) -> Status {
    Status::unavailable(format!("state: {e}"))
}

// ------------------------------------------------------------------------------ arguments

/// Text that is safe as one revision, ref or option value: bounded, no NUL or line break, and
/// (for `rev`) never an option.
pub(super) fn text_arg(text: &str, what: &str, rev: bool) -> Result<String, Status> {
    if text.len() > MAX_TEXT {
        return Err(bad(format!("{what} is longer than {MAX_TEXT} bytes")));
    }
    if text.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return Err(bad(format!("{what} holds a NUL or a line break")));
    }
    if rev && text.starts_with('-') {
        return Err(bad(format!("{what} must not start with '-'")));
    }
    Ok(text.to_owned())
}

/// Validated, literal path arguments, in order. A path must be a valid repository path and valid
/// UTF-8 (the child receives it as a command-line argument).
pub(super) fn path_args(raw: &[Bytes]) -> Result<Vec<OsString>, Status> {
    let paths = parse_paths(raw)?;
    let mut out = Vec::with_capacity(paths.len());
    let mut total = 0usize;
    for p in &paths {
        let text =
            std::str::from_utf8(p.as_bytes()).map_err(|_| bad("a path is not valid UTF-8"))?;
        total += text.len() + 1;
        out.push(OsString::from(text));
    }
    if total > MAX_ARG_BYTES {
        return Err(bad(format!(
            "the paths exceed {MAX_ARG_BYTES} bytes; ask for the whole tree"
        )));
    }
    Ok(out)
}

pub(super) fn os(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

// ------------------------------------------------------------------------------ child asc

pub(super) fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap_or_default())
}

/// The command that runs `asc <args>` in the workspace root. The root is verified first.
pub(super) fn command(m: &Mounted, args: &[OsString]) -> Result<tokio::process::Command, Status> {
    let root = PathBuf::from(&m.row.ws.mount);
    super::service::check_mount_root(&root).map_err(Status::permission_denied)?;
    let exe =
        std::env::current_exe().map_err(|e| internal(format!("cannot find the asc binary: {e}")))?;
    let mut c = tokio::process::Command::new(exe);
    c.args(args)
        .current_dir(&root)
        .env("ASC_NO_DAEMON", "1")
        .env("ASC_NO_ALIASES", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    Ok(c)
}

async fn read_capped<R: AsyncRead + Unpin>(r: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.take(cap as u64 + 1).read_to_end(&mut buf).await?;
    let over = buf.len() > cap;
    buf.truncate(cap);
    Ok((buf, over))
}

fn failure(cmd: &str, code: Option<i32>, stderr: &[u8]) -> Status {
    let text = String::from_utf8_lossy(stderr);
    let text: String = text.trim().chars().take(512).collect();
    Status::failed_precondition(format!(
        "asc {cmd} failed ({}): {text}",
        code.map_or("signal".into(), |c| c.to_string())
    ))
}

/// Runs a command to its end and returns its stdout.
pub(super) async fn run_collect(
    m: &Mounted,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>, Status> {
    let cmd = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut c = command(m, &args)?;
    c.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = c
        .spawn()
        .map_err(|e| internal(format!("cannot run asc {cmd}: {e}")))?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(internal("the child has no pipes"));
    };
    let input = child.stdin.take();
    let work = async {
        let feed = async {
            if let (Some(mut pipe), Some(data)) = (input, stdin) {
                let _ = pipe.write_all(&data).await;
                let _ = pipe.shutdown().await;
            }
        };
        let (o, e, ()) = tokio::join!(read_capped(out, MAX_OUT), read_capped(err, MAX_ERR), feed);
        (o, e)
    };
    let Ok((o, e)) = tokio::time::timeout(RUN_LIMIT, work).await else {
        let _ = child.kill().await;
        return Err(Status::deadline_exceeded(format!(
            "asc {cmd} ran longer than {} s",
            RUN_LIMIT.as_secs()
        )));
    };
    let ((stdout, over), (stderr, _)) = (o.map_err(internal)?, e.map_err(internal)?);
    if over {
        let _ = child.kill().await;
        return Err(Status::resource_exhausted(format!(
            "the answer of asc {cmd} exceeds {MAX_OUT} bytes"
        )));
    }
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .map_err(|_| Status::deadline_exceeded(format!("asc {cmd} did not end")))?
        .map_err(internal)?;
    if !status.success() {
        return Err(failure(&cmd, status.code(), &stderr));
    }
    Ok(stdout)
}

/// One JSON line as a message of the given type.
pub(super) fn decode_line(name: &str, line: &[u8]) -> Result<DynamicMessage, Status> {
    let value: Value = serde_json::from_slice(line.trim_ascii())
        .map_err(|e| internal(format!("asc printed bad JSON: {e}")))?;
    let desc = pool()
        .get_message_by_name(name)
        .ok_or_else(|| internal(format!("missing message {name}")))?;
    asc_rpc::json::decode(&desc, &value)
        .map_err(|e| internal(format!("asc printed a bad {name}: {e}")))
}

pub(super) fn first_line(out: &[u8]) -> Result<&[u8], Status> {
    out.split(|b| *b == b'\n')
        .find(|l| !l.trim_ascii().is_empty())
        .ok_or_else(|| internal("asc printed nothing"))
}

async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<usize, Status> {
    let mut total = 0;
    loop {
        let chunk = r.fill_buf().await.map_err(internal)?;
        if chunk.is_empty() {
            return Ok(total);
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if buf.len() + take > MAX_LINE {
            return Err(Status::resource_exhausted(format!(
                "a line of the answer exceeds {MAX_LINE} bytes"
            )));
        }
        buf.extend_from_slice(&chunk[..take]);
        r.consume(take);
        total += take;
        if done {
            return Ok(total);
        }
    }
}

type Conv<T> = fn(DynamicMessage) -> Result<T, Status>;

async fn pump<T: Send + 'static>(
    mut c: tokio::process::Command,
    cmd: &str,
    message: &str,
    conv: Conv<T>,
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
) -> Result<(), Status> {
    c.stdin(Stdio::null());
    let mut child = c
        .spawn()
        .map_err(|e| internal(format!("cannot run asc {cmd}: {e}")))?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(internal("the child has no pipes"));
    };
    let errors = tokio::spawn(read_capped(err, MAX_ERR));
    let mut out = BufReader::new(out);
    let deadline = tokio::time::Instant::now() + RUN_LIMIT;
    let (mut total, mut line) = (0usize, Vec::new());
    loop {
        line.clear();
        let n = match tokio::time::timeout_at(deadline, read_line_capped(&mut out, &mut line)).await
        {
            Ok(n) => n?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(Status::deadline_exceeded(format!(
                    "asc {cmd} ran longer than {} s",
                    RUN_LIMIT.as_secs()
                )));
            }
        };
        if n == 0 {
            break;
        }
        total += n;
        if total > MAX_STREAM {
            let _ = child.kill().await;
            return Err(Status::resource_exhausted(format!(
                "the answer of asc {cmd} exceeds {MAX_STREAM} bytes"
            )));
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        let item = conv(decode_line(message, &line)?)?;
        if tx.send(Ok(item)).await.is_err() {
            // The client went away: stop the child.
            let _ = child.kill().await;
            return Ok(());
        }
    }
    let status = child.wait().await.map_err(internal)?;
    if !status.success() {
        let stderr = errors
            .await
            .ok()
            .and_then(Result::ok)
            .map(|(b, _)| b)
            .unwrap_or_default();
        return Err(failure(cmd, status.code(), &stderr));
    }
    Ok(())
}

/// Runs `asc <args>` and streams each JSON line as `T`. The child stops when the client does.
pub(super) fn stream_asc<T: Send + 'static>(
    m: &Mounted,
    args: Vec<OsString>,
    message: &'static str,
    conv: Conv<T>,
) -> Result<PluginStream<T>, Status> {
    let cmd = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    let c = command(m, &args)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<T, Status>>(4);
    tokio::spawn(async move {
        if let Err(status) = pump(c, &cmd, message, conv, &tx).await {
            let _ = tx.send(Err(status)).await;
        }
    });
    Ok(
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
            .boxed(),
    )
}

fn conv_diff(d: DynamicMessage) -> Result<wire::DiffResponse, Status> {
    d.transcode_to().map_err(internal)
}
fn conv_log(d: DynamicMessage) -> Result<wire::LogResponse, Status> {
    d.transcode_to().map_err(internal)
}
pub(super) fn conv_blame(d: DynamicMessage) -> Result<wire::BlameResponse, Status> {
    d.transcode_to().map_err(internal)
}

pub(super) fn mounted(state: &State, ws_id: &str) -> Result<Arc<Mounted>, Status> {
    state.mounted(parse_id(ws_id)?)
}

// ------------------------------------------------------------------------------ Status

pub async fn status(
    state: &Arc<State>,
    r: wire::WsStatusRequest,
) -> Result<wire::WsStatusResponse, Status> {
    let m = mounted(state, &r.ws_id)?;
    if r.ignored {
        return Err(bad("ignored files are not part of the JSON status"));
    }
    let mut args = os(&[
        "status",
        "--json",
        if r.untracked {
            "--untracked-files=all"
        } else {
            "--untracked-files=no"
        },
        "--",
    ]);
    args.extend(path_args(&r.paths)?);
    // The token is taken first, so an event after the status is never lost.
    let token = current_pos(state, &m).await?.to_string();
    let out = run_collect(&m, args, None).await?;
    let mut resp: wire::WsStatusResponse =
        decode_line("asc.v1.WsStatusResponse", first_line(&out)?)?
            .transcode_to()
            .map_err(internal)?;
    resp.token = token;
    Ok(resp)
}

// -------------------------------------------------------------------------------- Diff

pub async fn diff(
    state: &Arc<State>,
    r: wire::WsDiffRequest,
) -> Result<PluginStream<wire::DiffResponse>, Status> {
    let m = mounted(state, &r.ws_id)?;
    let format = wire::DiffFormat::try_from(r.format).map_err(|_| bad("unknown diff format"))?;
    let mut args = os(&["diff", "--json"]);
    args.push(
        match format {
            wire::DiffFormat::Names => "--name-only",
            wire::DiffFormat::Status => "--name-status",
            wire::DiffFormat::Stat => "--stat",
            wire::DiffFormat::Numstat => "--numstat",
            wire::DiffFormat::Patch => "-p",
            wire::DiffFormat::Raw => "--raw",
        }
        .into(),
    );
    if r.context > 0 {
        args.push(format!("-U{}", r.context.min(MAX_CONTEXT)).into());
    }
    let side = |s: &str, default: &'static str| -> Result<String, Status> {
        Ok(if s.is_empty() {
            default.to_owned()
        } else {
            text_arg(s, "a diff side", false)?
        })
    };
    let (from, to) = (side(&r.from, "index")?, side(&r.to, "worktree")?);
    let named = |s: &str| matches!(s, "index" | "worktree");
    match (from.as_str(), to.as_str()) {
        ("index", "worktree") => {}
        (rev, "worktree") if !named(rev) => args.push(text_arg(rev, "a diff side", true)?.into()),
        (rev, "index") if !named(rev) => {
            args.push("--cached".into());
            args.push(text_arg(rev, "a diff side", true)?.into());
        }
        (a, b) if named(a) || named(b) => {
            return Err(bad(
                "'index' and 'worktree' only combine as index to work tree, or after a revision",
            ));
        }
        (a, b) => {
            args.push(text_arg(a, "a diff side", true)?.into());
            args.push(text_arg(b, "a diff side", true)?.into());
        }
    }
    args.push("--".into());
    args.extend(path_args(&r.paths)?);
    stream_asc(&m, args, "asc.v1.DiffResponse", conv_diff)
}

// ---------------------------------------------------------------------------------- Log

pub async fn log(
    state: &Arc<State>,
    r: wire::WsLogRequest,
) -> Result<PluginStream<wire::LogResponse>, Status> {
    let m = mounted(state, &r.ws_id)?;
    let q = r.query.unwrap_or_default();
    if q.count_only || !q.page_token.is_empty() || !q.resume_token.is_empty() {
        return Err(bad(
            "count_only and paging tokens are not served by the workspace log",
        ));
    }
    if q.include.len() + q.exclude.len() > MAX_ITEMS || q.paths.len() > MAX_ITEMS {
        return Err(bad(format!("more than {MAX_ITEMS} revisions or paths")));
    }
    let mut args = os(&["log", "--json"]);
    if q.first_parent {
        args.push("--first-parent".into());
    }
    if q.follow {
        args.push("--follow".into());
    }
    if q.skip > 0 {
        args.push(format!("--skip={}", q.skip).into());
    }
    let limit = if q.limit == 0 {
        DEFAULT_LOG
    } else {
        q.limit.min(MAX_LOG)
    };
    args.push(format!("-n{limit}").into());
    if !q.author.is_empty() {
        args.push(format!("--author={}", text_arg(&q.author, "the author", false)?).into());
    }
    if !q.grep.is_empty() {
        args.push(format!("--grep={}", text_arg(&q.grep, "the grep text", false)?).into());
    }
    if q.since > 0 {
        args.push(format!("--since={}", q.since).into());
    }
    if q.until > 0 {
        args.push(format!("--until={}", q.until).into());
    }
    if !q.pickaxe.is_empty() {
        let text =
            std::str::from_utf8(&q.pickaxe).map_err(|_| bad("the pickaxe is not valid UTF-8"))?;
        let text = text_arg(text, "the pickaxe", false)?;
        args.push(format!("{}{text}", if q.pickaxe_regex { "-G" } else { "-S" }).into());
    }
    for rev in &q.include {
        args.push(text_arg(rev, "a revision", true)?.into());
    }
    for rev in &q.exclude {
        args.push(format!("^{}", text_arg(rev, "a revision", true)?).into());
    }
    args.push("--".into());
    for p in &q.paths {
        let p = text_arg(p, "a path", false)?;
        RepoPath::new(p.as_bytes().to_vec()).map_err(|_| bad("invalid path"))?;
        args.push(p.into());
    }
    stream_asc(&m, args, "asc.v1.LogResponse", conv_log)
}

// -------------------------------------------------------------------------------- Blame

pub async fn blame(
    state: &Arc<State>,
    r: wire::WsBlameRequest,
) -> Result<PluginStream<wire::BlameResponse>, Status> {
    if !r.buffer.is_empty() {
        return super::plugin_api_stack::blame_buffer(state, r).await;
    }
    let m = mounted(state, &r.ws_id)?;
    let mut args = os(&["blame", "--json"]);
    if r.ignore_whitespace {
        args.push("-w".into());
    }
    if r.start_line > 0 {
        let end = if r.end_line == 0 {
            String::new()
        } else {
            r.end_line.to_string()
        };
        args.push(format!("-L{},{end}", r.start_line).into());
    } else if r.end_line > 0 {
        return Err(bad("end_line needs start_line"));
    }
    if !r.rev.is_empty() {
        args.push(text_arg(&r.rev, "the revision", true)?.into());
    }
    args.push("--".into());
    let path = path_args(std::slice::from_ref(&r.path))?;
    if path.len() != 1 {
        return Err(bad("blame needs one path"));
    }
    args.extend(path);
    stream_asc(&m, args, "asc.v1.BlameResponse", conv_blame)
}

// ----------------------------------------------------------------------------- Affected

fn ecosystem_name(e: i32) -> Result<&'static str, Status> {
    Ok(
        match wire::Ecosystem::try_from(e).map_err(|_| bad("unknown ecosystem"))? {
            wire::Ecosystem::Rust => "rust",
            wire::Ecosystem::Python => "python",
            wire::Ecosystem::Dotnet => "dotnet",
            wire::Ecosystem::Node => "node",
            wire::Ecosystem::Declared => "declared",
            wire::Ecosystem::Unspecified => return Err(bad("an unspecified ecosystem")),
        },
    )
}

fn ecosystem_of(name: &str) -> wire::Ecosystem {
    match name {
        "rust" => wire::Ecosystem::Rust,
        "python" => wire::Ecosystem::Python,
        "dotnet" => wire::Ecosystem::Dotnet,
        "node" => wire::Ecosystem::Node,
        "declared" => wire::Ecosystem::Declared,
        _ => wire::Ecosystem::Unspecified,
    }
}

fn unhex(text: &str) -> Bytes {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Bytes::new();
    }
    let digit = |b: u8| char::from(b).to_digit(16).unwrap_or(0) as u8;
    text.as_bytes()
        .chunks(2)
        .map(|p| digit(p[0]) << 4 | digit(p[1]))
        .collect::<Vec<u8>>()
        .into()
}

/// The `targets` object of one project of `asc affected --json` (SPEC §24.8.2, A4). An entry
/// that fails the `.asc/projects.toml` limits is dropped, never passed on.
fn targets_of(p: &Value) -> std::collections::HashMap<String, wire::TargetCmd> {
    let Some(map) = p.get("targets").and_then(Value::as_object) else {
        return Default::default();
    };
    map.iter()
        .take(asc_graph::targets::MAX_TARGETS)
        .filter_map(|(t, v)| {
            let argv: Vec<String> = v
                .get("argv")?
                .as_array()?
                .iter()
                .map(|w| w.as_str().map(str::to_owned))
                .collect::<Option<_>>()?;
            asc_graph::targets::check_argv(t, &argv).ok()?;
            Some((t.clone(), wire::TargetCmd { argv }))
        })
        .collect()
}

/// The SPEC §12A.9 document that `asc affected --json` prints, as an `AffectedResponse`.
fn affected_of(doc: &Value) -> wire::AffectedResponse {
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let list = |k: &str| -> Vec<String> {
        doc.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let projects = doc
        .get("projects")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|p| wire::AffectedProject {
                    project: Some(wire::ProjectInfo {
                        id: text(p, "id"),
                        ecosystem: ecosystem_of(&text(p, "ecosystem")) as i32,
                        name: Bytes::from(text(p, "name")),
                        root: Bytes::from(text(p, "root")),
                        kind: match text(p, "kind").as_str() {
                            "workspace" => wire::ProjectKind::Workspace,
                            "solution" => wire::ProjectKind::Solution,
                            _ => wire::ProjectKind::Package,
                        } as i32,
                        ..Default::default()
                    }),
                    direct: p.get("direct").and_then(Value::as_bool).unwrap_or(false),
                    reasons: p
                        .get("reasons")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    input_hash: unhex(&text(p, "hash")),
                    targets: targets_of(p),
                })
                .collect()
        })
        .unwrap_or_default();
    let issues = doc
        .get("issues")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|i| wire::DetectIssue {
                    path: Bytes::from(text(i, "path")),
                    message: text(i, "message"),
                })
                .collect()
        })
        .unwrap_or_default();
    wire::AffectedResponse {
        base: unhex(&text(doc, "base")),
        head: unhex(&text(doc, "head")),
        merge_base: unhex(&text(doc, "mergeBase")),
        all: doc.get("all").and_then(Value::as_bool).unwrap_or(false),
        projects,
        removed: list("removed"),
        unowned: list("unowned").into_iter().map(Bytes::from).collect(),
        issues,
        changed_paths: 0,
    }
}

pub async fn affected(
    state: &Arc<State>,
    r: wire::WsAffectedRequest,
) -> Result<wire::WsAffectedResponse, Status> {
    let m = mounted(state, &r.ws_id)?;
    let q = r.query.unwrap_or_default();
    if q.types.len() > MAX_ITEMS || q.select.len() > MAX_ITEMS {
        return Err(bad(format!(
            "more than {MAX_ITEMS} ecosystems or selectors"
        )));
    }
    let mut args = os(&["affected", "--json"]);
    if !q.base.is_empty() {
        args.push("--base".into());
        args.push(text_arg(&q.base, "the base", true)?.into());
    }
    if !q.head.is_empty() {
        args.push("--head".into());
        args.push(text_arg(&q.head, "the head", true)?.into());
    }
    if q.no_merge_base {
        args.push("--no-merge-base".into());
    }
    if r.uncommitted {
        args.push("--uncommitted".into());
    }
    if !q.types.is_empty() {
        let names = q
            .types
            .iter()
            .map(|t| ecosystem_name(*t))
            .collect::<Result<Vec<_>, _>>()?;
        args.push(format!("--type={}", names.join(",")).into());
    }
    if !q.target.is_empty() {
        args.push(format!("--target={}", text_arg(&q.target, "the target", true)?).into());
    }
    for s in &q.select {
        args.push(format!("--select={}", text_arg(s, "a selector", true)?).into());
    }
    if q.with_hashes {
        args.push("--hashes".into());
    }
    let out = run_collect(&m, args, None).await?;
    let doc: Value = serde_json::from_slice(first_line(&out)?)
        .map_err(|e| internal(format!("asc printed bad JSON: {e}")))?;
    Ok(wire::WsAffectedResponse {
        result: Some(affected_of(&doc)),
    })
}

// -------------------------------------------------------------------------------- Stage

/// Runs `base` once per batch of `paths` whose command line stays under [`MAX_ARG_BYTES`].
async fn run_batches(m: &Mounted, base: &[&str], paths: Vec<OsString>) -> Result<(), Status> {
    let mut batch: Vec<OsString> = Vec::new();
    let mut size = 0usize;
    let flush = |batch: &mut Vec<OsString>| {
        let mut args = os(base);
        args.append(batch);
        args
    };
    for p in paths {
        if size + p.len() + 1 > MAX_ARG_BYTES && !batch.is_empty() {
            run_collect(m, flush(&mut batch), None).await?;
            size = 0;
        }
        size += p.len() + 1;
        batch.push(p);
    }
    if !batch.is_empty() {
        run_collect(m, flush(&mut batch), None).await?;
    }
    Ok(())
}

pub async fn stage(
    state: &Arc<State>,
    r: wire::WsStageRequest,
) -> Result<wire::WsStageResponse, Status> {
    let m = mounted(state, &r.ws_id)?;
    if r.patch.len() > MAX_PATCH {
        return Err(Status::resource_exhausted(format!(
            "the patch exceeds {MAX_PATCH} bytes"
        )));
    }
    let batch_of = |raw: &[Bytes]| -> Result<Vec<OsString>, Status> {
        let paths = parse_paths(raw)?;
        paths
            .iter()
            .map(|p| {
                std::str::from_utf8(p.as_bytes())
                    .map(OsString::from)
                    .map_err(|_| bad("a path is not valid UTF-8"))
            })
            .collect()
    };
    let (remove, add) = (batch_of(&r.remove)?, batch_of(&r.add)?);
    {
        // The same gate as the other changes to the work tree.
        let _held = m.gate.lock().await;
        run_batches(&m, &["restore", "--staged", "--"], remove).await?;
        run_batches(&m, &["add", "-A", "--"], add).await?;
        if !r.patch.is_empty() {
            run_collect(&m, os(&["apply", "--cached", "-"]), Some(r.patch.to_vec())).await?;
        }
    }
    let status = self::status(
        state,
        wire::WsStatusRequest {
            ws_id: r.ws_id.clone(),
            untracked: true,
            ..Default::default()
        },
    )
    .await?;
    Ok(wire::WsStageResponse {
        status: Some(status),
    })
}

// ---------------------------------------------------------------------------------- Watch

/// Where a watcher is: one component per source, so a token resumes each independently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pos {
    epoch: u64,
    journal: u64,
    refs: i64,
    ops: i64,
    head: u64,
    stage: u64,
}

impl std::fmt::Display for Pos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "v1.{}.{}.{}.{}.{:016x}.{:016x}",
            self.epoch, self.journal, self.refs, self.ops, self.head, self.stage
        )
    }
}

impl Pos {
    fn parse(text: &str) -> Result<Self, Status> {
        let wrong = || bad("malformed watch token");
        if text.len() > MAX_TOKEN {
            return Err(wrong());
        }
        let parts: Vec<&str> = text.split('.').collect();
        let &[v, epoch, journal, refs, ops, head, stage] = parts.as_slice() else {
            return Err(wrong());
        };
        let dec = |s: &str| {
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                s.parse::<u64>().map_err(|_| wrong())
            } else {
                Err(wrong())
            }
        };
        let hex = |s: &str| {
            if s.len() == 16 {
                u64::from_str_radix(s, 16).map_err(|_| wrong())
            } else {
                Err(wrong())
            }
        };
        if v != "v1" {
            return Err(wrong());
        }
        Ok(Self {
            epoch: dec(epoch)?,
            journal: dec(journal)?,
            refs: i64::try_from(dec(refs)?).map_err(|_| wrong())?,
            ops: i64::try_from(dec(ops)?).map_err(|_| wrong())?,
            head: hex(head)?,
            stage: hex(stage)?,
        })
    }
}

fn print_of(data: &[u8]) -> u64 {
    let d = Sha256::digest(data);
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

fn head_print(head: &str, base: Option<ChangesetId>) -> u64 {
    let mut data = head.as_bytes().to_vec();
    data.push(0);
    if let Some(b) = base {
        data.extend_from_slice(b.as_bytes());
    }
    print_of(&data)
}

async fn stage_print(state: &State, m: &Mounted) -> Result<u64, Status> {
    let rows = stage::overlay(&state.db, m.id, StageSet::Default)
        .await
        .map_err(unavailable)?;
    if rows.len() > MAX_STAGE_ROWS {
        // Too many to hash every poll: the count stands in for the contents.
        return Ok(print_of(&(rows.len() as u64).to_be_bytes()));
    }
    Ok(print_of(format!("{rows:?}").as_bytes()))
}

async fn max_seq(state: &State, table: &'static str, repo: RepoKey) -> Result<i64, Status> {
    let sql = match table {
        "ref_log" => "SELECT COALESCE(MAX(seq), 0) FROM ref_log WHERE repo_id = ?",
        _ => "SELECT COALESCE(MAX(seq), 0) FROM op_log WHERE repo_id = ?",
    };
    sqlx::query_scalar::<_, i64>(sql)
        .bind(repo.0)
        .fetch_one(state.db.pool())
        .await
        .map_err(unavailable)
}

/// The position right now.
async fn current_pos(state: &State, m: &Mounted) -> Result<Pos, Status> {
    let jt = m.journal.token().await.map_err(unavailable)?;
    let row = workspaces::get(&state.db, m.id)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| Status::not_found("the workspace is gone"))?;
    Ok(Pos {
        epoch: jt.epoch,
        journal: jt.seq,
        refs: max_seq(state, "ref_log", m.row.ws.repo).await?,
        ops: max_seq(state, "op_log", m.row.ws.repo).await?,
        head: head_print(&row.ws.head, row.ws.base_cs),
        stage: stage_print(state, m).await?,
    })
}

fn event(kind: wire::watch_event::Kind, pos: &Pos) -> wire::WatchEvent {
    wire::WatchEvent {
        kind: kind as i32,
        token: pos.to_string(),
        ..Default::default()
    }
}

async fn git_of(state: &State, repo: RepoKey, cs: ChangesetId) -> Bytes {
    match git_map::asc_to_git(&state.db, repo, cs.as_bytes()).await {
        Ok(rows) => rows
            .into_iter()
            .find(|(_, k)| *k == ObjectKind::Commit)
            .map(|(o, _)| Bytes::copy_from_slice(o.as_bytes()))
            .unwrap_or_default(),
        Err(_) => Bytes::new(),
    }
}

async fn git_of_target(state: &State, repo: RepoKey, raw: Option<Vec<u8>>) -> Bytes {
    match raw.map(|r| RefTarget::from_bytes(&r)) {
        Some(Ok(RefTarget::Changeset(cs))) => git_of(state, repo, cs).await,
        _ => Bytes::new(),
    }
}

/// What the watcher last saw of HEAD.
struct Seen {
    head: String,
    base: Option<ChangesetId>,
}

/// One poll: the events since `pos`, which advances as each is produced.
async fn poll(
    state: &State,
    m: &Mounted,
    pos: &mut Pos,
    seen: &mut Option<Seen>,
    scan: Option<&ScanCtl>,
    overflowed: &mut bool,
) -> Result<Vec<wire::WatchEvent>, Status> {
    use wire::watch_event::Kind;
    let mut events = Vec::new();
    let repo = m.row.ws.repo;

    // 1. Path hints.
    if scan.is_some_and(|s| s.overflow.load(Ordering::SeqCst)) && !*overflowed {
        *overflowed = true;
        events.push(event(Kind::Resync, pos));
    }
    let since = m
        .journal
        .since(Some(&Token {
            epoch: pos.epoch,
            seq: pos.journal,
        }))
        .await
        .map_err(unavailable)?;
    if since.fresh {
        *pos = Pos {
            epoch: since.token.epoch,
            journal: since.token.seq,
            ..*pos
        };
        events.push(event(Kind::Resync, pos));
    } else if since.token.seq != pos.journal {
        let mut seen_paths = HashSet::new();
        let paths: Vec<Bytes> = since
            .paths
            .into_iter()
            .filter(|e| seen_paths.insert(e.path.clone()))
            .map(|e| Bytes::from(e.path))
            .collect();
        pos.journal = since.token.seq;
        if paths.len() > MAX_EVENT_PATHS {
            events.push(event(Kind::Resync, pos));
        } else if !paths.is_empty() {
            let mut e = event(Kind::Paths, pos);
            e.paths = paths;
            events.push(e);
        }
    }

    // 2. Refs and HEAD, from the ref log.
    let row = workspaces::get(&state.db, m.id)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| Status::not_found("the workspace is gone"))?;
    let mut head_event = false;
    if max_seq(state, "ref_log", repo).await? > pos.refs {
        let rows = sqlx::query("SELECT seq, name, old, new FROM ref_log WHERE repo_id = ? AND seq > ? ORDER BY seq LIMIT ?")
            .bind(repo.0)
            .bind(pos.refs)
            .bind(MAX_REF_ROWS)
            .fetch_all(state.db.pool())
            .await
            .map_err(unavailable)?;
        for r in &rows {
            let (seq, name): (i64, String) = (
                r.try_get("seq").map_err(unavailable)?,
                r.try_get("name").map_err(unavailable)?,
            );
            let (old, new): (Option<Vec<u8>>, Option<Vec<u8>>) = (
                r.try_get("old").map_err(unavailable)?,
                r.try_get("new").map_err(unavailable)?,
            );
            pos.refs = seq;
            let is_head = name == row.ws.head;
            head_event |= is_head;
            let mut e = event(if is_head { Kind::Head } else { Kind::Ref }, pos);
            e.old = git_of_target(state, repo, old).await;
            e.new = git_of_target(state, repo, new).await;
            e.r#ref = name;
            events.push(e);
        }
    }

    // 3. Head moves that no ref update explains: a switch or a detached move.
    let print = head_print(&row.ws.head, row.ws.base_cs);
    if print != pos.head {
        let ref_changed = seen.as_ref().is_none_or(|s| s.head != row.ws.head);
        pos.head = print;
        if !head_event {
            let mut e = event(Kind::Head, pos);
            e.r#ref = row.ws.head.clone();
            if let Some(old) = seen.as_ref().and_then(|s| s.base) {
                e.old = git_of(state, repo, old).await;
            }
            if let Some(new) = row.ws.base_cs {
                e.new = git_of(state, repo, new).await;
            }
            events.push(e);
        }
        if ref_changed {
            let mut e = event(Kind::SwitchDone, pos);
            e.r#ref = row.ws.head.clone();
            e.op = "switch".into();
            events.push(e);
        }
    }
    *seen = Some(Seen {
        head: row.ws.head.clone(),
        base: row.ws.base_cs,
    });

    // 4. Operations, from the op log.
    if max_seq(state, "op_log", repo).await? > pos.ops {
        let mut ops = oplog::list(&state.db, repo, MAX_OP_ROWS)
            .await
            .map_err(unavailable)?;
        ops.reverse();
        let first_kept = ops.first().map(|o| o.seq.0);
        if first_kept.is_some_and(|f| f > pos.ops + 1) && pos.ops > 0 {
            events.push(event(Kind::Resync, pos));
        }
        let from = pos.ops;
        for op in ops.into_iter().filter(|o| o.seq.0 > from) {
            let before = *pos;
            pos.ops = op.seq.0;
            if op.ws != m.id {
                continue;
            }
            let mut started = event(Kind::OpStarted, &before);
            started.op = op.kind.clone();
            events.push(started);
            let mut done = event(Kind::OpFinished, pos);
            done.op = op.kind.clone();
            events.push(done);
        }
    }

    // 5. Staging.
    let stage = stage_print(state, m).await?;
    if stage != pos.stage {
        pos.stage = stage;
        events.push(event(Kind::Stage, pos));
    }
    Ok(events)
}

/// Counts live `Watch` streams.
struct WatchSlot;

static WATCHERS: AtomicUsize = AtomicUsize::new(0);

impl WatchSlot {
    fn take() -> Result<Self, Status> {
        if WATCHERS.fetch_add(1, Ordering::SeqCst) >= MAX_WATCHERS {
            WATCHERS.fetch_sub(1, Ordering::SeqCst);
            return Err(Status::resource_exhausted(format!(
                "{MAX_WATCHERS} Watch streams are open already"
            )));
        }
        Ok(Self)
    }
}

impl Drop for WatchSlot {
    fn drop(&mut self) {
        WATCHERS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub async fn watch(
    state: &Arc<State>,
    r: wire::WatchRequest,
) -> Result<PluginStream<wire::WatchEvent>, Status> {
    use wire::watch_event::Kind;
    let m = mounted(state, &r.ws_id)?;
    let requested = if r.since_token.is_empty() {
        None
    } else {
        Some(Pos::parse(&r.since_token)?)
    };
    let slot = WatchSlot::take()?;
    let now = current_pos(state, &m).await?;
    let row = workspaces::get(&state.db, m.id)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| Status::not_found("the workspace is gone"))?;
    let mut pos = requested.unwrap_or(now);
    let mut first = Vec::new();
    // A token from a log that is behind it (state reset) cannot be honoured.
    if pos.refs > now.refs {
        pos.refs = now.refs;
        first.push(event(Kind::Resync, &pos));
    }
    if pos.ops > now.ops {
        pos.ops = now.ops;
        first.push(event(Kind::Resync, &pos));
    }
    let mut seen = requested.is_none().then(|| Seen {
        head: row.ws.head.clone(),
        base: row.ws.base_cs,
    });
    let guard = (m.row.ws.mode == Mode::Plain).then(|| attach_scanner(&m));
    if let Some((g, fresh)) = &guard {
        g.ctl.wait_ready().await;
        // A resumed stream that starts the scanner cannot know what changed in between.
        if *fresh && requested.is_some() {
            first.push(event(Kind::Resync, &pos));
        }
    }
    let guard = guard.map(|(g, _)| g);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<wire::WatchEvent, Status>>(256);
    let (state, id) = (Arc::clone(state), m.id);
    let weak = Arc::downgrade(&m);
    drop(m);
    tokio::spawn(async move {
        let _slot = slot;
        let mut overflowed = false;
        for e in first {
            if tx.send(Ok(e)).await.is_err() {
                return;
            }
        }
        let mut failures = 0u32;
        loop {
            tokio::select! {
                () = tx.closed() => return,
                () = tokio::time::sleep(POLL) => {}
            }
            // The stream ends when the workspace is unmounted (or replaced by a new mount).
            let Some(m) = weak
                .upgrade()
                .filter(|m| state.mounts.get(id).is_some_and(|c| Arc::ptr_eq(&c, m)))
            else {
                return;
            };
            match poll(
                &state,
                &m,
                &mut pos,
                &mut seen,
                guard.as_ref().map(|g| &*g.ctl),
                &mut overflowed,
            )
            .await
            {
                Ok(events) => {
                    failures = 0;
                    for e in events {
                        if tx.send(Ok(e)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(status) => {
                    failures += 1;
                    if failures >= MAX_POLL_FAILURES {
                        let _ = tx.send(Err(status)).await;
                        return;
                    }
                }
            }
        }
    });
    Ok(
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
            .boxed(),
    )
}

// -------------------------------------------------------------------------------- scanner

/// Shared by the watchers of one plain workspace.
struct ScanCtl {
    watchers: AtomicUsize,
    overflow: AtomicBool,
    /// The first scan (the baseline) is done, or the tree is too large to scan.
    ready: AtomicBool,
}

impl ScanCtl {
    /// Waits for the baseline scan, so an edit made after `Watch` returned is seen as an edit.
    async fn wait_ready(&self) {
        let deadline = tokio::time::Instant::now() + SCAN_WAIT;
        while !self.ready.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct ScanGuard {
    ctl: Arc<ScanCtl>,
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.ctl.watchers.fetch_sub(1, Ordering::SeqCst);
    }
}

static SCANNERS: LazyLock<Mutex<HashMap<WorkspaceId, Arc<ScanCtl>>>> =
    LazyLock::new(Mutex::default);

fn scanners() -> std::sync::MutexGuard<'static, HashMap<WorkspaceId, Arc<ScanCtl>>> {
    SCANNERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Registers a watcher of the plain workspace `m`, starting its scanner if none runs. The flag
/// says the scanner is new: edits made while nobody watched were not seen.
fn attach_scanner(m: &Arc<Mounted>) -> (ScanGuard, bool) {
    let mut map = scanners();
    let (ctl, fresh) = match map.get(&m.id) {
        Some(c) => (Arc::clone(c), false),
        None => {
            let c = Arc::new(ScanCtl {
                watchers: AtomicUsize::new(0),
                overflow: AtomicBool::new(false),
                ready: AtomicBool::new(false),
            });
            map.insert(m.id, Arc::clone(&c));
            tokio::spawn(run_scanner(Arc::downgrade(m), m.id, Arc::clone(&c)));
            (c, true)
        }
    };
    ctl.watchers.fetch_add(1, Ordering::SeqCst);
    (ScanGuard { ctl }, fresh)
}

/// What a scan keeps of a file.
type Snap = HashMap<Vec<u8>, (u64, u128)>;

fn stamp(meta: &std::fs::Metadata) -> (u64, u128) {
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    (meta.len(), nanos)
}

fn walk(dir: &Path, rel: &[u8], ign: &mut asc_wc::ignore::IgnoreStack, out: &mut Snap) -> bool {
    use asc_wc::safe_path::is_link;
    // Metadata only: a link is recorded as itself and never entered.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    }; // lint:allow-worktree-fs
    let _ = ign.push_dir(dir);
    for entry in entries.flatten() {
        let name = entry
            .file_name()
            .to_string_lossy()
            .into_owned()
            .into_bytes();
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let path = entry.path();
        let is_dir = meta.is_dir() && !is_link(&meta);
        if name == b".git" || ign.is_ignored(&path, is_dir) {
            continue;
        }
        let mut child = rel.to_vec();
        if !child.is_empty() {
            child.push(b'/');
        }
        child.extend_from_slice(&name);
        if is_dir {
            if !walk(&path, &child, ign, out) {
                return false;
            }
        } else {
            out.insert(child, stamp(&meta));
            if out.len() > MAX_SCAN_ENTRIES {
                return false;
            }
        }
    }
    ign.pop_dir();
    true
}

/// The files of the work tree at `root`, or `None` when there are too many.
fn scan_tree(root: &Path) -> Option<Snap> {
    let sources = {
        let common = asc_git::projection::Projection::open(root)
            .ok()
            .map(|p| p.common_dir().to_path_buf());
        asc_wc::ignore::IgnoreSources {
            asc_excludes: root.join(".asc").join("no-such-global-ignore"),
            core_excludes: None,
            local_exclude: root.join(".asc").join("info").join("exclude"),
            common_exclude: common.map_or_else(
                || root.join(".asc").join("info").join("exclude"),
                |c| c.join("info").join("exclude"),
            ),
        }
    };
    let mut ign = asc_wc::ignore::IgnoreStack::new(root, &sources);
    let mut out = Snap::new();
    walk(root, b"", &mut ign, &mut out).then_some(out)
}

/// Journal entries for the difference between two scans.
fn changes(old: &Snap, new: &Snap) -> Vec<(Op, Vec<u8>)> {
    let mut out = Vec::new();
    for (path, stat) in new {
        match old.get(path) {
            None => out.push((Op::Created, path.clone())),
            Some(prev) if prev != stat => out.push((Op::Modified, path.clone())),
            Some(_) => {}
        }
    }
    out.extend(
        old.keys()
            .filter(|p| !new.contains_key(*p))
            .map(|p| (Op::Deleted, p.clone())),
    );
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

async fn run_scanner(m: Weak<Mounted>, id: WorkspaceId, ctl: Arc<ScanCtl>) {
    let finish = |ctl: &Arc<ScanCtl>| {
        let mut map = scanners();
        if ctl.watchers.load(Ordering::SeqCst) == 0 {
            map.remove(&id);
            true
        } else {
            false
        }
    };
    let Some(root) = m.upgrade().map(|m| PathBuf::from(&m.row.ws.mount)) else {
        scanners().remove(&id);
        return;
    };
    let scan = |root: PathBuf| async move {
        tokio::task::spawn_blocking(move || scan_tree(&root))
            .await
            .ok()
            .flatten()
    };
    let Some(mut last) = scan(root.clone()).await else {
        ctl.overflow.store(true, Ordering::SeqCst);
        ctl.ready.store(true, Ordering::SeqCst);
        scanners().remove(&id);
        return;
    };
    ctl.ready.store(true, Ordering::SeqCst);
    let mut every = SCAN_MIN;
    loop {
        tokio::time::sleep(every).await;
        let Some(mount) = m.upgrade() else {
            scanners().remove(&id);
            return;
        };
        if ctl.watchers.load(Ordering::SeqCst) == 0 && finish(&ctl) {
            return;
        }
        let started = Instant::now();
        let Some(now) = scan(root.clone()).await else {
            ctl.overflow.store(true, Ordering::SeqCst);
            scanners().remove(&id);
            return;
        };
        for (op, path) in changes(&last, &now) {
            mount.journal.record(op, &path);
        }
        last = now;
        every = (started.elapsed() * 3).clamp(SCAN_MIN, SCAN_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_junk() {
        let p = Pos {
            epoch: 3,
            journal: 17,
            refs: 40,
            ops: 2,
            head: 0xdead_beef,
            stage: 7,
        };
        assert_eq!(Pos::parse(&p.to_string()).unwrap(), p);
        for junk in [
            "",
            "v1",
            "v2.1.1.1.1.0000000000000000.0000000000000000",
            "v1.-1.1.1.1.0000000000000000.0000000000000000",
            "v1.1.1.1.1.zz.0000000000000000",
            &"1".repeat(300),
        ] {
            assert!(Pos::parse(junk).is_err(), "{junk}");
        }
    }

    #[test]
    fn revisions_are_never_options() {
        assert!(text_arg("--output=x", "rev", true).is_err());
        assert!(text_arg("a\nb", "rev", true).is_err());
        assert!(text_arg("a\0b", "rev", true).is_err());
        assert!(text_arg(&"x".repeat(MAX_TEXT + 1), "rev", true).is_err());
        assert!(text_arg("HEAD~2", "rev", true).is_ok());
        assert!(text_arg("--x", "grep", false).is_ok());
    }

    #[test]
    fn paths_are_validated_and_bounded() {
        assert!(path_args(&[Bytes::from_static(b"a/b.txt")]).is_ok());
        assert!(path_args(&[Bytes::from_static(b"../x")]).is_err());
        assert!(path_args(&[Bytes::from_static(b"/abs")]).is_err());
        assert!(path_args(&[Bytes::from_static(&[0xff, 0xfe])]).is_err());
        let many: Vec<Bytes> = (0..2000)
            .map(|i| Bytes::from(format!("dir/file-{i:05}.txt")))
            .collect();
        assert!(path_args(&many).is_err());
    }

    #[test]
    fn scan_differences_become_journal_ops() {
        let mut old = Snap::new();
        old.insert(b"a".to_vec(), (1, 1));
        old.insert(b"b".to_vec(), (1, 1));
        old.insert(b"c".to_vec(), (1, 1));
        let mut new = Snap::new();
        new.insert(b"a".to_vec(), (1, 1));
        new.insert(b"b".to_vec(), (2, 1));
        new.insert(b"d".to_vec(), (1, 1));
        let got = changes(&old, &new);
        assert_eq!(
            got,
            vec![
                (Op::Modified, b"b".to_vec()),
                (Op::Deleted, b"c".to_vec()),
                (Op::Created, b"d".to_vec())
            ]
        );
    }

    #[test]
    fn the_affected_document_maps_to_the_response() {
        let doc = serde_json::json!({
            "version": 1, "base": "00", "head": "0a0b", "mergeBase": "", "all": false,
            "projects": [{"id": "node:a", "ecosystem": "node", "name": "a", "root": "pkgs/a", "kind": "package",
                          "direct": true, "reasons": ["pkgs/a/src/index.js"], "hash": null}],
            "removed": ["x"], "unowned": ["README.md"], "issues": [{"path": "p", "message": "m"}],
        });
        let r = affected_of(&doc);
        assert_eq!(&r.head[..], &[0x0a_u8, 0x0b][..]);
        assert_eq!(r.projects.len(), 1);
        let p = r.projects[0].project.as_ref().unwrap();
        assert_eq!(
            (p.id.as_str(), p.ecosystem),
            ("node:a", wire::Ecosystem::Node as i32)
        );
        assert!(r.projects[0].direct && r.projects[0].input_hash.is_empty());
        assert_eq!(
            (r.removed, r.unowned.len(), r.issues.len()),
            (vec!["x".to_string()], 1, 1)
        );
    }
}
