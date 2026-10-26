//! Local write tools of `asc mcp` (SPEC 22.3.3, 22.5; unit M10).
//!
//! Every verb tool runs `asc <verb> [--json]` as a child of this binary (`current_exe`): null
//! stdin, piped stdout and stderr (so a hook that prints cannot reach the protocol channel),
//! `GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=false`, no aliases, literal pathspecs. The user's
//! environment is otherwise inherited, so hooks run as they do for `git commit`.
//!
//! **Arguments.** Each tool's arguments become a fixed argv here and nowhere else. Free text
//! (messages, revisions, change ids) travels in `--flag=value` form, and a value that begins with
//! `-` is refused where it would be a positional, so no argument can become a flag. `--force`
//! of `undo` and `absorb` is never passed; `undo` always names its op (`--op`), so a later
//! operation by the user makes it fail with `STALE_OP`.
//!
//! **Cancellation.** A child runs in its own process group (Windows: a new process group inside
//! a kill-on-close job object). A cancelled or overlong call gets `SIGTERM` (Windows:
//! `CTRL_BREAK_EVENT`), 3 s to finish, then the whole group or job is killed. Lock recovery then
//! removes an `index.lock` or ref lock created after the child started (no process of the group
//! is left to hold one). The supervisor is its own task: the handler drops the call future the
//! moment a call is cancelled, and the task still finishes the shutdown.
//!
//! **Results.** Every verb runs with `--json` and the tool returns an `OpResult`. `create`,
//! `modify`, `undo`, `redo` and `worktree add` print one themselves. `restack`, `absorb`,
//! `sync`, `submit` and `hide` print a report of their own, which [`shaped`] maps to an
//! `OpResult` with the same code `Bridge.Run` uses.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use asc_mcp::backend::{CallContext, Reply};
use asc_mcp::catalogue::{Needs, ToolDef, WriteLevel};
use asc_mcp::errors::{ToolError, code, redact};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::bridge::shape;
use crate::cli::common::Ctx;

/// Longest commit message.
const MAX_MESSAGE: usize = 64 * 1024;
/// Longest revision, change id, path or other word.
const MAX_WORD: usize = 4096;
/// Most revisions of one `hide` or `unhide`.
const MAX_HIDE: usize = 1024;
/// Most reviewers of one `submit`.
const MAX_REVIEWERS: usize = 64;
/// Time a child gets between the polite stop and the kill.
const GRACE: Duration = Duration::from_secs(3);
/// How long one child may run.
const LIMIT: Duration = Duration::from_secs(300);
/// Most stdout bytes kept.
const MAX_OUT: usize = 8 * 1024 * 1024;
/// Most stderr bytes kept.
const MAX_ERR: usize = 16 * 1024;
/// Longest error text in a result.
const MAX_ERROR_TEXT: usize = 1024;
/// Most entries one lock-recovery walk looks at.
const MAX_LOCK_SCAN: usize = 20_000;

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new("INVALID_ARGUMENT", msg)
}

// ------------------------------------------------------------------------------ gating

/// Whether `def` may run at this write level (defence in depth; the handler lists by the same
/// rule). `uploads` is whether the workspace uploads drafts at once.
#[must_use]
pub fn permitted(def: &ToolDef, level: WriteLevel, uploads: bool) -> bool {
    match def.needs {
        Needs::WriteLocal => level >= WriteLevel::Local,
        Needs::WriteRemote => level >= WriteLevel::Remote,
        Needs::WriteLocalOrRemote if uploads => level >= WriteLevel::Remote,
        Needs::WriteLocalOrRemote => level >= WriteLevel::Local,
        _ => false,
    }
}

/// Whether this workspace uploads the drafts its verbs make (SPEC 22.3.3, "What local means").
///
/// The server's own flags are not known to the client without a round trip, so this answers
/// from what the client holds and errs towards `true`, which needs the stricter `write=remote`:
/// no remote configured, `asc.cloud=off`, `asc.branchless=false` or no daemon (`auto` needs one)
/// mean no upload; any other workspace with a remote may upload.
#[must_use]
pub fn workspace_uploads(ctx: &Ctx) -> bool {
    use crate::branchless::mode::{CloudSetting, Settings};
    let getenv = |k: &str| std::env::var(k).ok();
    let Ok(s) = Settings::resolve(&ctx.config, &ctx.env, &getenv) else {
        // Unreadable settings: assume the worst.
        return has_remote(ctx);
    };
    if s.cloud == CloudSetting::Off || !has_remote(ctx) {
        return false;
    }
    if s.cloud == CloudSetting::Auto {
        let no_daemon = crate::daemon::lifecycle::disabled(&|k| std::env::var_os(k));
        return s.branchless && !no_daemon;
    }
    true
}

fn has_remote(ctx: &Ctx) -> bool {
    let file = ctx.config.file();
    file.sections_by_name("remote")
        .into_iter()
        .flatten()
        .any(|sec| sec.body().value("url").is_some())
}

// ------------------------------------------------------------------------------ arguments

fn no_ctl(key: &str, s: &str) -> Result<(), ToolError> {
    if s.chars()
        .any(|c| c == '\0' || (c.is_control() && c != '\n' && c != '\t'))
    {
        return Err(bad(format!("{key} holds a control character")));
    }
    Ok(())
}

fn opt_text<'a>(args: &'a Value, key: &str, max: usize) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => {
            if s.len() > max {
                return Err(bad(format!("{key} is too long")));
            }
            no_ctl(key, s)?;
            Ok(Some(s))
        }
        Some(_) => Err(bad(format!("{key} must be a string"))),
    }
}

/// A revision, change id or path word: one line, no leading `-`.
fn opt_word<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    let v = opt_text(args, key, MAX_WORD)?;
    if let Some(s) = v {
        if s.starts_with('-') {
            return Err(bad(format!("{key} must not start with '-'")));
        }
        if s.contains(['\n', '\t']) {
            return Err(bad(format!("{key} must be one line")));
        }
    }
    Ok(v)
}

fn flag(args: &Value, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(format!("{key} must be a boolean"))),
    }
}

fn opt_flag(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad(format!("{key} must be a boolean"))),
    }
}

fn os(s: impl Into<OsString>) -> OsString {
    s.into()
}

/// What to run for one tool.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// `argv[0]` is the verb; the executable is not part of it.
    pub argv: Vec<OsString>,
}

fn op_arg(args: &Value) -> Result<String, ToolError> {
    let op = opt_text(args, "op", 64)?.ok_or_else(|| bad("op is required"))?;
    let ok = (4..=36).contains(&op.len()) && op.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !ok {
        return Err(bad("op must be an operation id from an earlier result"));
    }
    Ok(op.to_owned())
}

/// A path for a new worktree: relative to the worktree or absolute, never inside `.git` or `.asc`.
fn new_worktree_path(p: &str) -> Result<(), ToolError> {
    if p.starts_with('-') || p.starts_with("\\\\.\\") || p.starts_with("\\\\?\\") {
        return Err(bad("path is not a usable directory name"));
    }
    for c in Path::new(p).components() {
        if let Component::Normal(n) = c {
            let n = n.to_string_lossy().to_ascii_lowercase();
            if n == ".git" || n == ".asc" {
                return Err(bad("path is inside a repository metadata directory"));
            }
        }
    }
    Ok(())
}

/// The argv of a verb tool, from its validated arguments.
///
/// # Errors
/// An argument has the wrong type, is too long, holds a control character, or would read as a
/// flag.
pub fn plan(tool: &str, args: &Value) -> Result<Plan, ToolError> {
    let mut argv: Vec<OsString> = Vec::new();
    match tool {
        "create" => {
            let m = opt_text(args, "message", MAX_MESSAGE)?;
            let m = m
                .filter(|m| !m.trim().is_empty())
                .ok_or_else(|| bad("message is required: no editor runs"))?;
            argv.extend([os("create"), os("--json"), os(format!("--message={m}"))]);
            if flag(args, "all")? {
                argv.push(os("-a"));
            }
            if let Some(v) = opt_word(args, "onto")? {
                argv.push(os(format!("--onto={v}")));
            }
            if flag(args, "insert")? {
                argv.push(os("--insert"));
            }
        }
        "modify" => {
            argv.extend([os("modify"), os("--json")]);
            if let Some(m) = opt_text(args, "message", MAX_MESSAGE)? {
                argv.push(os(format!("--message={m}")));
            }
            if flag(args, "all")? {
                argv.push(os("-a"));
            }
            let into = opt_word(args, "into")?;
            let new_change = flag(args, "new_change")?;
            if into.is_some() && new_change {
                return Err(bad("into and new_change exclude each other"));
            }
            if let Some(v) = into {
                argv.push(os(format!("--into={v}")));
            }
            if new_change {
                argv.push(os("-c"));
            }
        }
        "absorb" => {
            argv.extend([os("absorb"), os("--json")]);
            if !flag(args, "apply")? {
                argv.push(os("--dry-run"));
            }
            if let Some(v) = opt_word(args, "into")? {
                argv.push(os(format!("--into={v}")));
            }
        }
        "restack" => {
            argv.extend([os("restack"), os("--json")]);
            match opt_text(args, "scope", 16)?.unwrap_or("all") {
                "all" => {}
                "downstack" => argv.push(os("--downstack")),
                "upstack" => argv.push(os("--upstack")),
                "only" => argv.push(os("--only")),
                _ => return Err(bad("scope must be all, downstack, upstack or only")),
            }
            if let Some(v) = opt_word(args, "change")? {
                argv.push(os(v));
            }
        }
        "undo" | "redo" => {
            argv.extend([
                os(tool),
                os("--json"),
                os(format!("--op={}", op_arg(args)?)),
            ]);
            if flag(args, "dry_run")? {
                argv.push(os("--dry-run"));
            }
        }
        "sync" => {
            argv.extend([os("sync"), os("--json")]);
            if opt_flag(args, "fetch")? == Some(false) {
                argv.push(os("--no-fetch"));
            }
            if opt_flag(args, "restack")? == Some(false) {
                argv.push(os("--no-restack"));
            }
            if flag(args, "delete_merged")? {
                argv.push(os("--delete"));
            }
        }
        "submit" => {
            argv.extend([os("submit"), os("--json")]);
            if flag(args, "whole_stack")? {
                argv.push(os("--stack"));
            }
            match opt_flag(args, "draft")? {
                Some(true) => argv.push(os("--draft")),
                Some(false) => argv.push(os("--publish")),
                None => {}
            }
            if let Some(list) = args.get("reviewers").filter(|v| !v.is_null()) {
                let list = list
                    .as_array()
                    .ok_or_else(|| bad("reviewers must be an array"))?;
                if list.len() > MAX_REVIEWERS {
                    return Err(bad("too many reviewers"));
                }
                for r in list {
                    let r = r.as_str().ok_or_else(|| bad("reviewers must be strings"))?;
                    let ok = !r.is_empty()
                        && r.len() <= 128
                        && !r.starts_with('-')
                        && r.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c));
                    if !ok {
                        return Err(bad(
                            "a reviewer name has characters a user name cannot have",
                        ));
                    }
                    argv.push(os(format!("--reviewer={r}")));
                }
            }
            if flag(args, "update_only")? {
                argv.push(os("--update-only"));
            }
            if flag(args, "force")? {
                argv.push(os("--force"));
            }
            if let Some(v) = opt_word(args, "change")? {
                argv.push(os(v));
            }
        }
        "hide" | "unhide" => {
            // B05: `asc hide <rev>...` and `asc unhide <id>...`, `--json` OpResult, undoable.
            argv.extend([os(tool), os("--json")]);
            if flag(args, "dry_run")? {
                argv.push(os("--dry-run"));
            }
            let key = if tool == "hide" { "revs" } else { "ids" };
            let list = args
                .get(key)
                .filter(|v| !v.is_null())
                .and_then(Value::as_array)
                .ok_or_else(|| bad(format!("{key} must be a non-empty array")))?;
            if list.is_empty() || list.len() > MAX_HIDE {
                return Err(bad(format!("{key} holds 1 to {MAX_HIDE} entries")));
            }
            // An entry that begins with `-` is refused, so none can become a flag.
            for v in list {
                let v = v
                    .as_str()
                    .ok_or_else(|| bad(format!("{key} must be strings")))?;
                if v.is_empty() || v.len() > MAX_WORD || v.starts_with('-') {
                    return Err(bad(format!(
                        "{key} entries are revisions or change prefixes"
                    )));
                }
                if v.chars().any(char::is_control) {
                    return Err(bad(format!("{key} holds a control character")));
                }
                argv.push(os(v));
            }
        }
        "worktree_create" => {
            argv.extend([os("worktree"), os("add"), os("--json")]);
            let ephemeral = flag(args, "ephemeral")?;
            let ttl = opt_word(args, "ttl")?;
            if ttl.is_some() && !ephemeral {
                return Err(bad("ttl needs ephemeral"));
            }
            if ephemeral {
                argv.push(os("--ephemeral"));
            }
            if let Some(t) = ttl {
                let ok = t.len() <= 16 && t.chars().all(|c| c.is_ascii_alphanumeric());
                if !ok {
                    return Err(bad("ttl is a number and s, m, h, d or w"));
                }
                argv.push(os(format!("--ttl={t}")));
            }
            match opt_text(args, "mode", 16)? {
                None => {}
                Some(m @ ("virtual" | "plain")) => argv.push(os(format!("--mode={m}"))),
                Some(_) => return Err(bad("mode is virtual or plain")),
            }
            let path = opt_text(args, "path", MAX_WORD)?.ok_or_else(|| bad("path is required"))?;
            new_worktree_path(path)?;
            argv.push(os(path));
            if let Some(rev) = opt_word(args, "rev")? {
                argv.push(os(rev));
            }
        }
        other => {
            return Err(ToolError::not_available(format!(
                "{other} is not a verb tool"
            )));
        }
    }
    Ok(Plan { argv })
}

// ------------------------------------------------------------------------------ stage inputs

/// Every repository path a `stage` call names: `add`, `remove` and the paths in the headers of
/// `patch`. The caller passes each through the path gate. A patch whose headers cannot be read
/// with certainty is refused.
///
/// # Errors
/// An argument has the wrong type, or a patch header is quoted or cannot be split.
pub fn stage_paths(args: &Value) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::new();
    for key in ["add", "remove"] {
        match args.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Array(a)) => {
                if a.len() > 4096 {
                    return Err(bad(format!("{key} lists too many paths")));
                }
                for p in a {
                    out.push(
                        p.as_str()
                            .ok_or_else(|| bad(format!("{key} must hold strings")))?
                            .to_owned(),
                    );
                }
            }
            Some(_) => return Err(bad(format!("{key} must be an array"))),
        }
    }
    match args.get("patch") {
        None | Some(Value::Null) => {}
        Some(Value::String(p)) => patch_paths(p, &mut out)?,
        Some(_) => return Err(bad("patch must be a string")),
    }
    Ok(out)
}

fn strip<'a>(p: &'a str, prefix: &str) -> &'a str {
    p.strip_prefix(prefix).unwrap_or(p)
}

fn patch_paths(patch: &str, out: &mut Vec<String>) -> Result<(), ToolError> {
    let quoted = || {
        ToolError::new(
            code::PATH_POLICY,
            "quoted or unreadable paths in a patch are refused",
        )
    };
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if rest.contains('"') {
                return Err(quoted());
            }
            // Every split at " b/" is a candidate pair; all must pass the gate.
            let mut any = false;
            for (i, _) in rest.match_indices(" b/") {
                any = true;
                out.push(strip(&rest[..i], "a/").to_owned());
                out.push(
                    rest[i + 1..]
                        .strip_prefix("b/")
                        .unwrap_or(&rest[i + 1..])
                        .to_owned(),
                );
            }
            if !any {
                return Err(quoted());
            }
        } else if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            let rest = rest.split('\t').next().unwrap_or(rest);
            if rest.starts_with('"') {
                return Err(quoted());
            }
            if rest != "/dev/null" {
                out.push(strip(strip(rest, "a/"), "b/").to_owned());
            }
        } else if let Some(rest) = ["rename from ", "rename to ", "copy from ", "copy to "]
            .iter()
            .find_map(|k| line.strip_prefix(k))
        {
            if rest.starts_with('"') {
                return Err(quoted());
            }
            out.push(rest.to_owned());
        }
    }
    if out.len() > 8192 {
        return Err(bad("the patch names too many paths"));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ child process

/// A finished child.
#[derive(Debug)]
pub struct Finished {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    over: bool,
}

async fn drain<R: AsyncRead + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut over = false;
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if n > room {
                    over = true;
                }
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    (kept, over)
}

fn clean(s: &str, max: usize) -> String {
    redact(
        &s.chars()
            .filter(|c| !c.is_control() || *c == ' ')
            .take(max)
            .collect::<String>(),
    )
}

/// Runs one verb to its end. The supervision runs as its own task so that a cancelled call (whose
/// future the handler drops) still ends its child gracefully and recovers its locks.
async fn run(root: &Path, argv: Vec<OsString>, ctx: &CallContext) -> Result<Finished, ToolError> {
    let root = root.to_path_buf();
    let cancel = ctx.cancel.clone().cancelled_owned();
    tokio::spawn(supervise(root, argv, cancel))
        .await
        .map_err(|e| ToolError::internal(format!("the verb supervisor failed: {e}")))?
}

async fn supervise(
    root: PathBuf,
    argv: Vec<OsString>,
    cancel: impl std::future::Future<Output = ()> + Send,
) -> Result<Finished, ToolError> {
    tokio::pin!(cancel);
    let verb = argv
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    let exe = std::env::current_exe()
        .map_err(|e| ToolError::internal(format!("cannot find the asc binary: {e}")))?;
    let mut c = tokio::process::Command::new(exe);
    c.args(&argv)
        .current_dir(&root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "false")
        .env("GCM_INTERACTIVE", "never")
        .env("ASC_NO_ALIASES", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    c.process_group(0);
    #[cfg(windows)]
    c.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    let started = SystemTime::now();
    let mut child = c
        .spawn()
        .map_err(|e| ToolError::internal(format!("cannot run asc {verb}: {e}")))?;
    let group = group::Group::new(&child);
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        group.kill();
        return Err(ToolError::internal("the child has no pipes"));
    };
    let out_task = tokio::spawn(drain(out, MAX_OUT));
    let err_task = tokio::spawn(drain(err, MAX_ERR));

    let status = tokio::select! {
        r = child.wait() => Some(r),
        () = &mut cancel => None,
        () = tokio::time::sleep(LIMIT) => None,
    };
    let Some(status) = status else {
        // Polite stop, grace period, then the whole group or job.
        group.term();
        if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
            group.kill();
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        }
        group.kill();
        out_task.abort();
        err_task.abort();
        let freed = recover_locks(&root, started);
        if freed > 0 {
            eprintln!(
                "asc mcp: asc {verb}: removed {freed} stale lock file(s) left by the stopped child"
            );
        }
        return Err(ToolError::internal(format!(
            "asc {verb} was cancelled or ran longer than {} s; it was stopped and its locks recovered",
            LIMIT.as_secs()
        )));
    };
    let status = status.map_err(|e| ToolError::internal(e.to_string()))?;
    // A background grandchild may hold the pipes open: wait a bounded time, then let go.
    let (stdout, over) = tokio::time::timeout(Duration::from_secs(5), out_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let (stderr, _) = tokio::time::timeout(Duration::from_secs(5), err_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let code = status.code().unwrap_or(-1);
    if !stderr.is_empty() {
        eprintln!(
            "asc mcp: asc {verb}: {}",
            clean(&String::from_utf8_lossy(&stderr), MAX_ERROR_TEXT)
        );
    }
    // `group` goes out of scope here: on Windows that closes the kill-on-close job.
    Ok(Finished {
        code,
        stdout,
        stderr,
        over,
    })
}

#[cfg(unix)]
mod group {
    use rustix::process::{Pid, Signal, kill_process_group};

    /// The child's process group.
    pub struct Group(Option<Pid>);

    impl Group {
        pub fn new(child: &tokio::process::Child) -> Self {
            Self(
                child
                    .id()
                    .and_then(|p| i32::try_from(p).ok())
                    .and_then(Pid::from_raw),
            )
        }

        pub fn term(&self) {
            if let Some(p) = self.0 {
                let _ = kill_process_group(p, Signal::TERM);
            }
        }

        pub fn kill(&self) {
            if let Some(p) = self.0 {
                let _ = kill_process_group(p, Signal::KILL);
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 job objects and console events; every call is confined here.
mod group {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    const CTRL_BREAK_EVENT: u32 = 1;

    /// The child's process group (its pid) and a kill-on-close job holding it.
    pub struct Group {
        pid: u32,
        job: usize,
    }

    impl Group {
        pub fn new(child: &tokio::process::Child) -> Self {
            let pid = child.id().unwrap_or(0);
            let mut job = 0usize;
            if let Some(h) = child.raw_handle() {
                // SAFETY: a fresh job handle is owned by this struct and closed in `drop`; the
                // process handle is the live child's; the limit struct is zeroed plain data.
                unsafe {
                    let j: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                    if !j.is_null() {
                        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                        let set = SetInformationJobObject(
                            j,
                            JobObjectExtendedLimitInformation,
                            std::ptr::addr_of!(info).cast(),
                            u32::try_from(std::mem::size_of_val(&info)).unwrap_or(0),
                        );
                        if set != 0 && AssignProcessToJobObject(j, h as HANDLE) != 0 {
                            job = j as usize;
                        } else {
                            CloseHandle(j);
                        }
                    }
                }
            }
            Self { pid, job }
        }

        pub fn term(&self) {
            if self.pid != 0 {
                // SAFETY: plain Win32 call; the group id is the child's pid, never 0 (which
                // would reach every process on the console).
                unsafe {
                    GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid);
                }
            }
        }

        pub fn kill(&self) {
            if self.job != 0 {
                // SAFETY: the handle is the open job this struct owns.
                unsafe {
                    TerminateJobObject(self.job as HANDLE, 1);
                }
            }
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            if self.job != 0 {
                // SAFETY: closing the job handle this struct owns, once.
                unsafe {
                    CloseHandle(self.job as HANDLE);
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------ lock recovery

/// The git directory of the worktree at `root` and the common directory.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot = root.join(".git");
    let meta = std::fs::symlink_metadata(&dot).ok()?;
    let gitdir = if meta.is_dir() {
        dot
    } else if meta.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let p = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    } else {
        return None;
    };
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(t) => {
            let p = Path::new(t.trim());
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                gitdir.join(p)
            }
        }
        Err(_) => gitdir.clone(),
    };
    Some((gitdir, common))
}

fn stale_lock(p: &Path, since: SystemTime) -> bool {
    std::fs::symlink_metadata(p)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok())
        .is_some_and(|t| t >= since)
}

/// Removes the `index.lock` and ref locks created at or after `since`. The caller has ended every
/// process of the child's group, so none of them holds one.
fn recover_locks(root: &Path, since: SystemTime) -> usize {
    let Some((gitdir, common)) = git_dirs(root) else {
        return 0;
    };
    let mut found: Vec<PathBuf> = vec![
        gitdir.join("index.lock"),
        gitdir.join("HEAD.lock"),
        common.join("packed-refs.lock"),
    ];
    let mut stack = vec![(common.join("refs"), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            if seen > MAX_LOCK_SCAN {
                break;
            }
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && depth < 12 {
                stack.push((p, depth + 1));
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "lock") {
                found.push(p);
            }
        }
    }
    found
        .into_iter()
        .filter(|p| stale_lock(p, since))
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

// ------------------------------------------------------------------------------ results

fn last_json(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        .filter(Value::is_object)
}

/// The `error` object for a failed verb: the child's own JSON error, else its stderr text.
fn error_result(f: &Finished) -> Value {
    let err_text = String::from_utf8_lossy(&f.stderr);
    let own = last_json(&f.stderr).or_else(|| last_json(&f.stdout));
    if let Some(e) = own.as_ref().and_then(|v| v.get("error")) {
        let c = e
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or(code::INTERNAL);
        let m = e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the command failed");
        let c = if c.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            c
        } else {
            code::INTERNAL
        };
        return json!({ "error": { "code": c, "message": clean(m, MAX_ERROR_TEXT) } });
    }
    let c = if err_text.contains("STALE_OP") {
        code::STALE_OP
    } else {
        code::INTERNAL
    };
    let m = clean(&err_text, MAX_ERROR_TEXT);
    let m = if m.is_empty() {
        format!("the command failed with exit code {}", f.code)
    } else {
        m
    };
    json!({ "error": { "code": c, "message": m } })
}

/// The `OpResult` form of `report`, what `tool` printed. A verb that prints an `OpResult`
/// itself (it has `rewritten`) is not touched; a report of an older verb is mapped by
/// `Bridge.Run`'s [`shape::adapt`]; any other shape stays as it is.
fn shaped(tool: &str, report: Value) -> Value {
    if report.get("rewritten").is_some() {
        return report;
    }
    shape::adapt(tool, &report).unwrap_or(report)
}

/// The result when a verb that exited `code` printed no result line: `ok` for a clean exit,
/// else `nothing` with the first words of its stderr as the hint.
fn silent_result(f: &Finished) -> Value {
    if f.code == 0 {
        return json!({ "version": 1, "status": "ok" });
    }
    let mut v = json!({ "version": 1, "status": "nothing" });
    let note = clean(&String::from_utf8_lossy(&f.stderr), MAX_ERROR_TEXT);
    if !note.is_empty() {
        v["hints"] = json!([note]);
    }
    v
}

fn reply_for(tool: &str, f: &Finished) -> Result<Reply, ToolError> {
    if f.over {
        return Err(ToolError::budget("the answer of the verb is too large"));
    }
    match f.code {
        0..=2 => Ok(Reply::Verb {
            exit_code: f.code,
            result: last_json(&f.stdout)
                .map_or_else(|| silent_result(f), |report| shaped(tool, report)),
        }),
        c => Ok(Reply::Verb {
            exit_code: c,
            result: error_result(f),
        }),
    }
}

/// Runs the write tool `tool` in the worktree at `root`.
///
/// # Errors
/// Invalid arguments, a child that cannot start, or one that was cancelled or ran too long.
pub async fn call(
    root: &Path,
    tool: &str,
    args: &Value,
    ctx: &CallContext,
) -> Result<Reply, ToolError> {
    let p = plan(tool, args)?;
    let f = run(root, p.argv, ctx).await?;
    reply_for(tool, &f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tool: &str, args: &Value) -> Vec<String> {
        plan(tool, args)
            .unwrap()
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn create_needs_a_message_and_passes_it_as_one_argument() {
        assert!(plan("create", &json!({})).is_err());
        assert!(plan("create", &json!({"message": "  "})).is_err());
        let a = argv(
            "create",
            &json!({"message": "-x --force\nbody", "all": true, "insert": true}),
        );
        assert_eq!(
            a,
            [
                "create",
                "--json",
                "--message=-x --force\nbody",
                "-a",
                "--insert"
            ]
        );
    }

    #[test]
    fn hide_and_unhide_pass_flags_first_and_entries_last() {
        assert_eq!(
            argv("hide", &json!({"revs": ["abc", "def"], "dry_run": true})),
            ["hide", "--json", "--dry-run", "abc", "def"]
        );
        assert_eq!(
            argv("unhide", &json!({"ids": ["kk"]})),
            ["unhide", "--json", "kk"]
        );
    }

    #[test]
    fn hide_refuses_flag_like_empty_and_control_revisions() {
        for bad in [
            json!("-x"),
            json!(""),
            json!(
                "a
b"
            ),
            json!(1),
        ] {
            assert!(plan("hide", &json!({"revs": [bad]})).is_err());
        }
        assert!(plan("hide", &json!({})).is_err());
        assert!(plan("hide", &json!({"revs": []})).is_err());
        assert!(plan("unhide", &json!({"ids": "kk"})).is_err());
        let many: Vec<String> = (0..1025).map(|i| format!("r{i}")).collect();
        assert!(plan("unhide", &json!({"ids": many})).is_err());
    }

    #[test]
    fn values_that_look_like_flags_are_refused() {
        for (tool, args) in [
            ("create", json!({"message": "m", "onto": "--help"})),
            ("modify", json!({"into": "-x"})),
            ("restack", json!({"change": "--upstack"})),
            ("submit", json!({"change": "-f"})),
            ("submit", json!({"reviewers": ["--force"]})),
            ("worktree_create", json!({"path": "-b"})),
            ("worktree_create", json!({"path": "w", "rev": "--detach"})),
        ] {
            assert!(plan(tool, &args).is_err(), "{tool} {args}");
        }
    }

    #[test]
    fn undo_always_names_its_op_and_never_forces() {
        assert!(plan("undo", &json!({})).is_err());
        assert!(plan("undo", &json!({"op": "--force"})).is_err());
        assert!(plan("undo", &json!({"op": "x y"})).is_err());
        let a = argv(
            "undo",
            &json!({"op": "0123abcd", "dry_run": true, "force": true}),
        );
        assert_eq!(a, ["undo", "--json", "--op=0123abcd", "--dry-run"]);
        assert_eq!(
            argv("redo", &json!({"op": "0123abcd"})),
            ["redo", "--json", "--op=0123abcd"]
        );
    }

    #[test]
    fn absorb_previews_unless_applied() {
        assert_eq!(
            argv("absorb", &json!({})),
            ["absorb", "--json", "--dry-run"]
        );
        assert_eq!(
            argv("absorb", &json!({"apply": true})),
            ["absorb", "--json"]
        );
    }

    #[test]
    fn restack_scopes() {
        assert_eq!(argv("restack", &json!({})), ["restack", "--json"]);
        assert_eq!(
            argv("restack", &json!({"scope": "upstack", "change": "c1"})),
            ["restack", "--json", "--upstack", "c1"]
        );
        assert!(plan("restack", &json!({"scope": "sideways"})).is_err());
    }

    #[test]
    fn worktree_paths_stay_out_of_metadata_directories() {
        assert!(plan("worktree_create", &json!({"path": "../w/.git/x"})).is_err());
        assert!(plan("worktree_create", &json!({"path": "w", "ttl": "7d"})).is_err());
        let a = argv(
            "worktree_create",
            &json!({"path": "../w", "ephemeral": true, "ttl": "7d", "mode": "plain"}),
        );
        assert_eq!(
            a,
            [
                "worktree",
                "add",
                "--json",
                "--ephemeral",
                "--ttl=7d",
                "--mode=plain",
                "../w"
            ]
        );
    }

    #[test]
    fn stage_paths_come_from_lists_and_patch_headers() {
        let p = "diff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let got = stage_paths(&json!({"add": ["y"], "patch": p})).unwrap();
        assert!(got.contains(&"y".to_owned()) && got.contains(&"x.txt".to_owned()));
        let evil = "diff --git a/../x b/../x\n--- a/../x\n+++ b/../x\n";
        assert!(
            stage_paths(&json!({"patch": evil}))
                .unwrap()
                .contains(&"../x".to_owned())
        );
        assert!(stage_paths(&json!({"patch": "diff --git \"a/q\" \"b/q\"\n"})).is_err());
        assert!(stage_paths(&json!({"add": "x"})).is_err());
    }

    #[test]
    fn stale_locks_are_only_those_newer_than_the_child() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("index.lock"), b"").unwrap();
        std::fs::write(git.join("refs/heads/topic.lock"), b"").unwrap();
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(
            recover_locks(d.path(), future),
            0,
            "locks older than the child stay"
        );
        assert!(git.join("index.lock").exists());
        let past = SystemTime::now() - Duration::from_secs(3600);
        assert_eq!(recover_locks(d.path(), past), 2);
        assert!(!git.join("index.lock").exists() && !git.join("refs/heads/topic.lock").exists());
    }

    #[test]
    fn a_report_of_its_own_becomes_an_op_result_and_an_op_result_stays() {
        let sync = json!({
            "version": 1,
            "fast_forwarded": [{
                "trunk": "main",
                "old": "aa",
                "new": "bb",
                "to": "origin/main"
            }],
            "restacked": [],
        });
        let r = shaped("sync", sync);
        assert_eq!(r["status"], "ok", "{r}");
        assert_eq!(r["refs"][0]["name"], "refs/heads/main", "{r}");
        assert!(r["rewritten"].is_array() && r["hints"].is_array(), "{r}");
        let quiet = shaped("sync", json!({ "version": 1 }));
        assert_eq!(quiet["status"], "nothing", "{quiet}");

        let own = json!({ "version": 1, "status": "ok", "op": "x1", "rewritten": [], "refs": [] });
        assert_eq!(shaped("create", own.clone()), own);
        let unknown = json!({ "version": 1, "something": 1 });
        assert_eq!(shaped("worktree_create", unknown.clone()), unknown);
    }

    #[test]
    fn a_verb_that_printed_no_result_is_ok_on_exit_zero_and_nothing_otherwise() {
        let done = |code: i32, err: &[u8]| Finished {
            code,
            stdout: Vec::new(),
            stderr: err.to_vec(),
            over: false,
        };
        assert_eq!(silent_result(&done(0, b""))["status"], "ok");
        let stopped = silent_result(&done(1, b"hook said no"));
        assert_eq!(stopped["status"], "nothing");
        assert_eq!(stopped["hints"][0], "hook said no");
    }

    #[test]
    fn failures_carry_stale_op_and_no_secrets() {
        let f = Finished {
            code: 128,
            stdout: Vec::new(),
            stderr:
                br#"{"version":1,"error":{"code":"STALE_OP","message":"op is not the latest"}}"#
                    .to_vec(),
            over: false,
        };
        assert_eq!(error_result(&f)["error"]["code"], "STALE_OP");
        let g = Finished {
            code: 128,
            stdout: Vec::new(),
            stderr: b"fatal: https://user:hunter2@host/x (STALE_OP)".to_vec(),
            over: false,
        };
        let e = error_result(&g);
        assert_eq!(e["error"]["code"], "STALE_OP");
        assert!(!e.to_string().contains("hunter2"), "{e}");
    }
}
