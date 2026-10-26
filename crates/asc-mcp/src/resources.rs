//! MCP resources, resource templates and argument completion (SPEC 22.3.5).
//!
//! `resources/list` never enumerates repository content: it returns the workspace's `status` and
//! `smartlog` and nothing else. Everything else is an `asc://` URI built from a template. A URI is
//! untrusted input, so [`Resource::parse`] is strict: it accepts only the shapes of the table in
//! SPEC 22.3.5, decodes each path segment once, and refuses `..`, `.`, empty segments, separators
//! inside a decoded segment, control characters, drive letters and a leading `-` in a revision.
//!
//! Reads go through the same [`McpBackend`] tools as `tools/call` (`read_file`, `ls_tree`, `show`,
//! `change_info`, `status`, `smartlog`), so the backend's own gates (the path gate of SPEC 22.5)
//! apply. An `asc://ws/{ws}/...` URI names a worktree, which must be one of
//! [`McpBackend::worktrees`]; anything else is refused before the backend is called.
//!
//! TODO(M12a): the server backend scopes `owner/name` URIs through a `repo` argument that the
//! HTTP `ServeBackend` consumes; the local backend ignores it and serves the launch repository.
//! TODO(M05): `Handler::dispatch` does not yet route `resources/*` and `completion/complete`;
//! [`list`], [`templates`], [`read`] and [`complete`] are the entry points for that wiring.

use std::path::Path;

use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::backend::{CallContext, McpBackend, Reply};
use crate::errors::{INVALID_PARAMS, RpcError, ToolError};

/// The URI scheme.
pub const SCHEME: &str = "asc://";
/// Completion results are cut at this many entries.
pub const MAX_COMPLETIONS: usize = 50;
/// `ttlMs` for a revision that is a full object id.
pub const TTL_IMMUTABLE_MS: u64 = 86_400_000;
/// `ttlMs` for a movable revision or a change.
pub const TTL_MOVABLE_MS: u64 = 5_000;
/// Longest URI accepted.
pub const MAX_URI_BYTES: usize = 4096;

/// One resource, parsed from an `asc://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// `asc://{owner}/{name}/blob/{rev}/{+path}{?start_line,end_line}`
    Blob {
        /// Repository.
        repo: Repo,
        /// Revision.
        rev: String,
        /// Repository-relative path.
        path: String,
        /// First line, 1-based.
        start_line: Option<u64>,
        /// Last line, inclusive.
        end_line: Option<u64>,
    },
    /// `asc://{owner}/{name}/tree/{rev}/{+path}{?after}`
    Tree {
        /// Repository.
        repo: Repo,
        /// Revision.
        rev: String,
        /// Directory path; empty is the root.
        path: String,
        /// Paging token.
        after: Option<String>,
    },
    /// `asc://{owner}/{name}/commit/{rev}`
    Commit {
        /// Repository.
        repo: Repo,
        /// Revision.
        rev: String,
    },
    /// `asc://{owner}/{name}/change/{change}`
    Change {
        /// Repository.
        repo: Repo,
        /// Change id.
        change: String,
    },
    /// `asc://ws/{ws}/status`
    WsStatus {
        /// Worktree.
        ws: String,
    },
    /// `asc://ws/{ws}/smartlog`
    WsSmartlog {
        /// Worktree.
        ws: String,
    },
    /// `asc://ws/{ws}/file/{+path}`
    WsFile {
        /// Worktree.
        ws: String,
        /// Repository-relative path.
        path: String,
    },
}

/// `owner/name`; local-only repositories use `local/<directory name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Owner segment.
    pub owner: String,
    /// Repository name segment.
    pub name: String,
}

fn bad(msg: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, msg)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` once. The result must be valid UTF-8 without control characters.
fn decode(seg: &str) -> Result<String, RpcError> {
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1).copied().and_then(hex_val);
            let l = b.get(i + 2).copied().and_then(hex_val);
            match (h, l) {
                (Some(h), Some(l)) => out.push(h * 16 + l),
                _ => return Err(bad("malformed percent-encoding in resource URI")),
            }
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).map_err(|_| bad("resource URI is not valid UTF-8"))?;
    if s.chars().any(char::is_control) {
        return Err(bad("control character in resource URI"));
    }
    Ok(s)
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A revision or change reference: no option-like prefix, whitespace, separators or controls.
fn rev_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.starts_with('-')
        && !s.contains("..")
        && s.bytes().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'@' | b'~' | b'^' | b':')
        })
}

fn is_full_id(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// One decoded path component.
fn component_ok(c: &str) -> bool {
    !c.is_empty()
        && c != "."
        && c != ".."
        && !c.contains(['/', '\\'])
        && !c.chars().any(char::is_control)
        && !(c.len() == 2 && c.as_bytes()[1] == b':' && c.as_bytes()[0].is_ascii_alphabetic())
}

/// Joins raw (still encoded) segments into a repository-relative path.
fn path_of(segs: &[&str]) -> Result<String, RpcError> {
    let mut parts = Vec::with_capacity(segs.len());
    for s in segs {
        let d = decode(s)?;
        if !component_ok(&d) {
            return Err(bad(
                "path in resource URI is not a plain repository-relative path",
            ));
        }
        parts.push(d);
    }
    Ok(parts.join("/"))
}

/// Validates a path given as an argument (completion): same rules as a URI path.
fn plain_path(p: &str) -> bool {
    p.is_empty() || p.split('/').all(component_ok)
}

fn parse_query(q: &str, allowed: &[&str]) -> Result<Vec<(String, String)>, RpcError> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| bad("malformed query in resource URI"))?;
        let k = decode(k)?;
        if !allowed.contains(&k.as_str()) || out.iter().any(|(e, _)| *e == k) {
            return Err(bad(
                "unsupported or repeated query parameter in resource URI",
            ));
        }
        out.push((k, decode(v)?));
    }
    Ok(out)
}

fn line_no(v: &str) -> Result<u64, RpcError> {
    match v.parse::<u64>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(bad("line numbers are 1-based integers")),
    }
}

fn ws_ok(ws: &str) -> bool {
    !ws.is_empty() && ws.len() <= 1024
}

fn no_query(q: Option<&str>) -> Result<(), RpcError> {
    if q.is_some() {
        Err(bad("this resource takes no query"))
    } else {
        Ok(())
    }
}

fn rev_of(s: &str) -> Result<String, RpcError> {
    let r = decode(s)?;
    if rev_ok(&r) {
        Ok(r)
    } else {
        Err(bad("invalid revision in resource URI"))
    }
}

impl Resource {
    /// Parses and validates `uri`.
    ///
    /// # Errors
    /// `-32602` for any URI outside the SPEC 22.3.5 table or failing the checks above.
    pub fn parse(uri: &str) -> Result<Self, RpcError> {
        if uri.len() > MAX_URI_BYTES || uri.contains('#') {
            return Err(bad("unsupported resource URI"));
        }
        let rest = uri
            .strip_prefix(SCHEME)
            .ok_or_else(|| bad("resource URIs use the asc:// scheme"))?;
        let (path_part, query) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        let segs: Vec<&str> = path_part.split('/').collect();
        if segs[0] == "ws" {
            return Self::parse_ws(&segs, query);
        }
        if segs.len() < 4 {
            return Err(bad("unsupported resource URI"));
        }
        let owner = decode(segs[0])?;
        let name = decode(segs[1])?;
        if !name_ok(&owner) || !name_ok(&name) {
            return Err(bad("invalid owner or name in resource URI"));
        }
        let repo = Repo { owner, name };
        match (segs[2], segs.len()) {
            ("blob", n) if n >= 5 => {
                let mut start_line = None;
                let mut end_line = None;
                for (k, v) in parse_query(query.unwrap_or(""), &["start_line", "end_line"])? {
                    if k == "start_line" {
                        start_line = Some(line_no(&v)?);
                    } else {
                        end_line = Some(line_no(&v)?);
                    }
                }
                if let (Some(s), Some(e)) = (start_line, end_line)
                    && s > e
                {
                    return Err(bad("start_line is after end_line"));
                }
                Ok(Self::Blob {
                    repo,
                    rev: rev_of(segs[3])?,
                    path: path_of(&segs[4..])?,
                    start_line,
                    end_line,
                })
            }
            ("tree", n) if n >= 4 => {
                let mut after = None;
                for (_, v) in parse_query(query.unwrap_or(""), &["after"])? {
                    if v.len() > 1024 {
                        return Err(bad("after is too long"));
                    }
                    after = Some(v);
                }
                // A trailing slash (`tree/{rev}/`) is the root; interior empties are refused.
                let tail: &[&str] = if segs.len() > 4 && segs[segs.len() - 1].is_empty() {
                    &segs[4..segs.len() - 1]
                } else {
                    &segs[4..]
                };
                Ok(Self::Tree {
                    repo,
                    rev: rev_of(segs[3])?,
                    path: path_of(tail)?,
                    after,
                })
            }
            ("commit", 4) => {
                no_query(query)?;
                Ok(Self::Commit {
                    repo,
                    rev: rev_of(segs[3])?,
                })
            }
            ("change", 4) => {
                no_query(query)?;
                Ok(Self::Change {
                    repo,
                    change: rev_of(segs[3])?,
                })
            }
            _ => Err(bad("unsupported resource URI")),
        }
    }

    fn parse_ws(segs: &[&str], query: Option<&str>) -> Result<Self, RpcError> {
        if segs.len() < 3 {
            return Err(bad("unsupported resource URI"));
        }
        let ws = decode(segs[1])?;
        if !ws_ok(&ws) {
            return Err(bad("invalid workspace in resource URI"));
        }
        match (segs[2], segs.len()) {
            ("status", 3) => no_query(query).map(|()| Self::WsStatus { ws }),
            ("smartlog", 3) => no_query(query).map(|()| Self::WsSmartlog { ws }),
            ("file", n) if n > 3 => {
                no_query(query)?;
                Ok(Self::WsFile {
                    ws,
                    path: path_of(&segs[3..])?,
                })
            }
            _ => Err(bad("unsupported resource URI")),
        }
    }

    /// `ttlMs` (SPEC 22.3.5): a day for a full object id, five seconds for movable references, 0
    /// for workspace resources.
    #[must_use]
    pub fn ttl_ms(&self) -> u64 {
        match self {
            Self::Blob { rev, .. } | Self::Tree { rev, .. } | Self::Commit { rev, .. } => {
                if is_full_id(rev) {
                    TTL_IMMUTABLE_MS
                } else {
                    TTL_MOVABLE_MS
                }
            }
            Self::Change { .. } => TTL_MOVABLE_MS,
            Self::WsStatus { .. } | Self::WsSmartlog { .. } | Self::WsFile { .. } => 0,
        }
    }

    /// The workspace this resource names, if it is an `asc://ws/` one.
    #[must_use]
    pub fn workspace(&self) -> Option<&str> {
        match self {
            Self::WsStatus { ws } | Self::WsSmartlog { ws } | Self::WsFile { ws, .. } => Some(ws),
            _ => None,
        }
    }

    /// The backend tool and arguments that serve this resource.
    #[must_use]
    pub fn tool_call(&self) -> (&'static str, Value) {
        fn with_repo(repo: &Repo, mut a: Value) -> Value {
            a["repo"] = json!(format!("{}/{}", repo.owner, repo.name));
            a
        }
        match self {
            Self::Blob {
                repo,
                rev,
                path,
                start_line,
                end_line,
            } => {
                let mut a = json!({ "rev": rev, "path": path });
                if let Some(s) = start_line {
                    a["start_line"] = json!(s);
                }
                if let Some(e) = end_line {
                    a["end_line"] = json!(e);
                }
                ("read_file", with_repo(repo, a))
            }
            Self::Tree {
                repo,
                rev,
                path,
                after,
            } => {
                let mut a = json!({ "rev": rev, "path": path, "recursive": false });
                if let Some(x) = after {
                    a["after"] = json!(x);
                }
                ("ls_tree", with_repo(repo, a))
            }
            Self::Commit { repo, rev } => ("show", with_repo(repo, json!({ "rev": rev }))),
            Self::Change { repo, change } => (
                "change_info",
                with_repo(repo, json!({ "changes": [change] })),
            ),
            Self::WsStatus { ws } => ("status", json!({ "worktree": ws })),
            Self::WsSmartlog { ws } => ("smartlog", json!({ "worktree": ws })),
            Self::WsFile { ws, path } => (
                "read_file",
                json!({ "worktree": ws, "rev": "", "path": path }),
            ),
        }
    }
}

fn annotations() -> Value {
    json!({ "audience": ["assistant"] })
}

/// Percent-encodes everything but unreserved characters, for one URI segment.
#[must_use]
pub fn encode_segment(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The `resources/list` result: the workspace's `status` and `smartlog`, never repository content.
/// `ws` is the launch worktree; a server without a workspace passes `None` and lists nothing.
#[must_use]
pub fn list(ws: Option<&str>) -> Value {
    let Some(ws) = ws else {
        return json!({ "resources": [] });
    };
    let enc = encode_segment(ws);
    json!({ "resources": [
        { "uri": format!("asc://ws/{enc}/status"), "name": "status",
          "description": "Working-copy status of the workspace.", "mimeType": "application/json",
          "annotations": annotations() },
        { "uri": format!("asc://ws/{enc}/smartlog"), "name": "smartlog",
          "description": "Stacks of changes in the workspace.", "mimeType": "application/json",
          "annotations": annotations() },
    ] })
}

/// The `resources/templates/list` result (SPEC 22.3.5 table).
#[must_use]
pub fn templates() -> Value {
    let t = |tpl: &str, name: &str, desc: &str| json!({ "uriTemplate": tpl, "name": name, "description": desc, "annotations": annotations() });
    json!({ "resourceTemplates": [
        t("asc://{owner}/{name}/blob/{rev}/{+path}{?start_line,end_line}", "blob",
          "File text at a revision (read_file rules; binary files are refused)."),
        t("asc://{owner}/{name}/tree/{rev}/{+path}{?after}", "tree", "One non-recursive directory page at a revision."),
        t("asc://{owner}/{name}/commit/{rev}", "commit", "One changeset (show)."),
        t("asc://{owner}/{name}/change/{change}", "change", "One change (change_info)."),
        t("asc://ws/{ws}/status", "ws-status", "Working-copy status of a worktree."),
        t("asc://ws/{ws}/smartlog", "ws-smartlog", "Stacks of changes in a worktree."),
        t("asc://ws/{ws}/file/{+path}", "ws-file", "A working-copy file, through the path gate."),
    ] })
}

fn call_ctx(budget_bytes: u64, cancel: &CancellationToken) -> CallContext {
    CallContext {
        principal: None,
        budget_bytes,
        position: None,
        cancel: cancel.clone(),
        progress: None,
    }
}

/// Merges a message stream (repeated fields concatenated, scalars last-wins) within `budget`.
async fn collect(reply: Reply, budget: u64) -> Result<Value, ToolError> {
    match reply {
        Reply::Verb { exit_code, result } => match crate::errors::map_exit(exit_code, result) {
            crate::errors::Outcome::Ok(v) => Ok(v),
            crate::errors::Outcome::Err(e) => Err(e),
        },
        Reply::Messages(mut s) => {
            let mut merged = Map::new();
            let mut used = 0u64;
            while let Some(m) = s.next().await {
                let m = m?;
                used = used.saturating_add(m.to_string().len() as u64);
                if used > budget {
                    return Err(ToolError::budget(
                        "the resource is larger than the read budget; use a range or a page",
                    ));
                }
                if let Value::Object(o) = m {
                    for (k, v) in o {
                        match (merged.get_mut(&k), v) {
                            (Some(Value::Array(a)), Value::Array(b)) => a.extend(b),
                            (_, v) => {
                                merged.insert(k, v);
                            }
                        }
                    }
                }
            }
            Ok(Value::Object(merged))
        }
    }
}

async fn check_ws<B: McpBackend>(backend: &B, ws: &str) -> Result<(), ToolError> {
    let refuse = || ToolError::new("INVALID_ARGUMENT", "ws is not a worktree of this workspace");
    let wanted = std::fs::canonicalize(Path::new(ws)).map_err(|_| refuse())?;
    let known = backend.worktrees().await?;
    if known
        .iter()
        .any(|k| std::fs::canonicalize(k).is_ok_and(|k| k == wanted))
    {
        Ok(())
    } else {
        Err(refuse())
    }
}

/// Errors of [`read`]: a malformed URI is a JSON-RPC error, a failed read a tool error.
#[derive(Debug)]
pub enum ReadError {
    /// -32602.
    Rpc(RpcError),
    /// The backend or a gate refused.
    Tool(ToolError),
}

/// `resources/read`: parses `uri`, gates a workspace, reads through the backend and returns the
/// `contents` result with `ttlMs`, `cacheScope: "private"` and the assistant audience.
///
/// # Errors
/// [`ReadError::Rpc`] for a bad URI; [`ReadError::Tool`] for a refused, binary, oversize or failed
/// read.
pub async fn read<B: McpBackend>(
    backend: &B,
    uri: &str,
    budget_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Value, ReadError> {
    let res = Resource::parse(uri).map_err(ReadError::Rpc)?;
    if let Some(ws) = res.workspace() {
        check_ws(backend, ws).await.map_err(ReadError::Tool)?;
    }
    let (tool, args) = res.tool_call();
    let reply = backend
        .call(tool, &args, &call_ctx(budget_bytes, cancel))
        .await
        .map_err(ReadError::Tool)?;
    let v = collect(reply, budget_bytes)
        .await
        .map_err(ReadError::Tool)?;
    let (text, mime) = match &res {
        Resource::Blob { .. } | Resource::WsFile { .. } => {
            if v.get("binary").and_then(Value::as_bool).unwrap_or(false) {
                return Err(ReadError::Tool(ToolError::new(
                    "BINARY_FILE",
                    "binary files are not served as resources",
                )));
            }
            (
                v.get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                "text/plain",
            )
        }
        _ => (v.to_string(), "application/json"),
    };
    Ok(json!({ "contents": [{
        "uri": uri, "mimeType": mime, "text": text,
        "annotations": annotations(),
        "_meta": { "ttlMs": res.ttl_ms(), "cacheScope": "private" },
    }] }))
}

/// `completion/complete` for a `ref/resource` template: `rev` and `path` candidates, at most
/// [`MAX_COMPLETIONS`]. `path` lists the directory part of the typed value with `ls_tree` at the
/// `rev` (and `owner`/`name`) already given in `context.arguments`; `rev` offers `HEAD`. Nothing
/// else is completed, and values from `context` are validated like URI segments.
///
/// # Errors
/// `-32602` for a malformed request.
pub async fn complete<B: McpBackend>(
    backend: &B,
    params: &Value,
    budget_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Value, RpcError> {
    let arg = params
        .get("argument")
        .ok_or_else(|| bad("argument is required"))?;
    let name = arg
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("argument.name is required"))?;
    let typed = arg.get("value").and_then(Value::as_str).unwrap_or("");
    if typed.len() > 1024 || typed.chars().any(char::is_control) {
        return Err(bad("argument.value is invalid"));
    }
    if params.pointer("/ref/type").and_then(Value::as_str) != Some("ref/resource") {
        return Err(bad("only ref/resource completion is supported"));
    }
    let ctx_arg = |k: &str| {
        params
            .pointer(&format!("/context/arguments/{k}"))
            .and_then(Value::as_str)
    };
    let mut values: Vec<String> = Vec::new();
    match name {
        "rev" if "HEAD".starts_with(typed) => values.push("HEAD".to_owned()),
        "path" => {
            let rev = ctx_arg("rev")
                .filter(|r| rev_ok(r))
                .ok_or_else(|| bad("complete rev before path"))?;
            let (dir, prefix) = typed.rsplit_once('/').unwrap_or(("", typed));
            // `prefix` only filters names and is never joined into a path; `dir` is the path.
            if !plain_path(dir) {
                return Err(bad(
                    "argument.value is not a plain repository-relative path",
                ));
            }
            let mut a = json!({ "rev": rev, "path": dir, "recursive": false, "limit": 1000 });
            if let (Some(o), Some(n)) = (ctx_arg("owner"), ctx_arg("name")) {
                if !name_ok(o) || !name_ok(n) {
                    return Err(bad("invalid owner or name"));
                }
                a["repo"] = json!(format!("{o}/{n}"));
            }
            let reply = backend
                .call("ls_tree", &a, &call_ctx(budget_bytes, cancel))
                .await
                .map_err(|e| bad(&e.message))?;
            let v = collect(reply, budget_bytes)
                .await
                .map_err(|e| bad(&e.message))?;
            for e in v
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(n) = e
                    .get("name")
                    .or_else(|| e.get("path"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let n = n.rsplit('/').next().unwrap_or(n);
                if !component_ok(n) || !n.starts_with(prefix) {
                    continue;
                }
                let is_dir = matches!(
                    e.get("type").and_then(Value::as_str),
                    Some("tree" | "dir" | "directory")
                );
                let full = if dir.is_empty() {
                    n.to_owned()
                } else {
                    format!("{dir}/{n}")
                };
                values.push(if is_dir { format!("{full}/") } else { full });
            }
        }
        _ => {}
    }
    values.sort();
    let total = values.len();
    values.truncate(MAX_COMPLETIONS);
    Ok(
        json!({ "completion": { "values": values, "total": total, "hasMore": total > MAX_COMPLETIONS } }),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use futures::stream;

    use super::*;

    struct Fake {
        reply: Value,
        calls: Mutex<Vec<(String, Value)>>,
        worktrees: Vec<PathBuf>,
    }

    impl Fake {
        fn new(reply: Value) -> Self {
            Self {
                reply,
                calls: Mutex::new(vec![]),
                worktrees: vec![],
            }
        }
    }

    #[async_trait::async_trait]
    impl McpBackend for Fake {
        async fn call(
            &self,
            tool: &str,
            args: &Value,
            _: &CallContext,
        ) -> Result<Reply, ToolError> {
            self.calls
                .lock()
                .unwrap()
                .push((tool.to_owned(), args.clone()));
            Ok(Reply::Messages(Box::pin(stream::iter(vec![Ok(self
                .reply
                .clone())]))))
        }

        async fn worktrees(&self) -> Result<Vec<PathBuf>, ToolError> {
            Ok(self.worktrees.clone())
        }
    }

    fn rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn list_has_only_status_and_smartlog() {
        let v = list(Some("/w/a b"));
        let r = v["resources"].as_array().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["uri"], "asc://ws/%2Fw%2Fa%20b/status");
        assert_eq!(r[1]["name"], "smartlog");
        assert_eq!(r[0]["annotations"]["audience"][0], "assistant");
        assert_eq!(list(None)["resources"].as_array().unwrap().len(), 0);
        let s = list(Some("x")).to_string();
        assert!(!s.contains("blob") && !s.contains("tree"));
    }

    #[test]
    fn templates_match_spec() {
        let t = templates();
        let uris: Vec<&str> = t["resourceTemplates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["uriTemplate"].as_str().unwrap())
            .collect();
        assert_eq!(uris.len(), 7);
        assert!(uris.contains(&"asc://{owner}/{name}/blob/{rev}/{+path}{?start_line,end_line}"));
        assert!(uris.contains(&"asc://ws/{ws}/file/{+path}"));
    }

    #[test]
    fn parses_each_shape() {
        let id = "a".repeat(40);
        let r = Resource::parse(&format!(
            "asc://local/repo/blob/{id}/src/a%20b.rs?start_line=2&end_line=9"
        ))
        .unwrap();
        assert_eq!(r.ttl_ms(), TTL_IMMUTABLE_MS);
        let (tool, args) = r.tool_call();
        assert_eq!(tool, "read_file");
        assert_eq!(args["path"], "src/a b.rs");
        assert_eq!(args["start_line"], 2);
        let r = Resource::parse("asc://o/n/tree/main/").unwrap();
        assert_eq!(r.ttl_ms(), TTL_MOVABLE_MS);
        assert_eq!(r.tool_call().1["path"], "");
        assert_eq!(r.tool_call().1["recursive"], false);
        assert!(matches!(
            Resource::parse("asc://o/n/commit/HEAD~1").unwrap(),
            Resource::Commit { .. }
        ));
        assert!(matches!(
            Resource::parse("asc://o/n/change/abc").unwrap(),
            Resource::Change { .. }
        ));
        assert_eq!(Resource::parse("asc://ws/w/status").unwrap().ttl_ms(), 0);
        assert!(matches!(
            Resource::parse("asc://ws/w/file/a/b").unwrap(),
            Resource::WsFile { .. }
        ));
        assert!(matches!(
            Resource::parse("asc://o/n/tree/main/d?after=x").unwrap(),
            Resource::Tree { after: Some(_), .. }
        ));
    }

    #[test]
    fn refuses_traversal_and_malformed_uris() {
        for u in [
            "http://o/n/commit/x",
            "asc://o/n/blob/main/../secret",
            "asc://o/n/blob/main/a/%2e%2e/b",
            "asc://o/n/blob/main/a%2F..%2Fb",
            "asc://o/n/blob/main/a%5Cb",
            "asc://o/n/blob/main/./a",
            "asc://o/n/blob/main/a//b",
            "asc://o/n/blob/main/C:/x",
            "asc://o/n/blob/main/a%00b",
            "asc://o/n/blob/-rf/a",
            "asc://o/n/blob/a..b/a",
            "asc://o/n/blob/main/a?start_line=0",
            "asc://o/n/blob/main/a?start_line=5&end_line=2",
            "asc://o/n/blob/main/a?bogus=1",
            "asc://o/n/blob/main/a#frag",
            "asc://../n/commit/x",
            "asc://o/n/commit/",
            "asc://o/n/commit/x/extra",
            "asc://o/n/unknown/x",
            "asc://ws/w/status?x=1",
            "asc://ws/w/file",
            "asc://ws/w/file/../x",
            "asc://o/n/blob/main/a%zz",
        ] {
            assert!(Resource::parse(u).is_err(), "{u} must be refused");
        }
    }

    #[test]
    fn blob_read_returns_text_with_ttl_and_scope() {
        let b = Fake::new(json!({ "content": "hello\n" }));
        let v = rt(read(
            &b,
            "asc://o/n/blob/main/a.txt",
            1000,
            &CancellationToken::new(),
        ))
        .unwrap();
        let c = &v["contents"][0];
        assert_eq!(c["text"], "hello\n");
        assert_eq!(c["_meta"]["ttlMs"], 5000);
        assert_eq!(c["_meta"]["cacheScope"], "private");
        assert_eq!(c["annotations"]["audience"][0], "assistant");
        assert_eq!(b.calls.lock().unwrap()[0].0, "read_file");
    }

    #[test]
    fn binary_blob_is_refused() {
        let b = Fake::new(json!({ "binary": true, "content": "AAEC" }));
        let e = rt(read(
            &b,
            "asc://o/n/blob/main/a.bin",
            1000,
            &CancellationToken::new(),
        ))
        .unwrap_err();
        assert!(matches!(e, ReadError::Tool(t) if t.code == "BINARY_FILE"));
    }

    #[test]
    fn oversize_read_hits_the_budget() {
        let b = Fake::new(json!({ "content": "x".repeat(500) }));
        let e = rt(read(
            &b,
            "asc://o/n/blob/main/a",
            100,
            &CancellationToken::new(),
        ))
        .unwrap_err();
        assert!(matches!(e, ReadError::Tool(_)));
    }

    #[test]
    fn unknown_worktree_is_refused_before_the_backend() {
        let b = Fake::new(json!({}));
        let e = rt(read(
            &b,
            "asc://ws/%2Fnope%2Fnot-a-dir/status",
            1000,
            &CancellationToken::new(),
        ))
        .unwrap_err();
        assert!(matches!(e, ReadError::Tool(_)));
        assert!(b.calls.lock().unwrap().is_empty());
        // A real directory that is not a listed worktree is refused too.
        let dir = std::env::temp_dir();
        let uri = format!("asc://ws/{}/status", encode_segment(dir.to_str().unwrap()));
        assert!(rt(read(&b, &uri, 1000, &CancellationToken::new())).is_err());
        assert!(b.calls.lock().unwrap().is_empty());
        let mut ok = Fake::new(json!({ "files": [] }));
        ok.worktrees = vec![dir];
        rt(read(&ok, &uri, 1000, &CancellationToken::new())).unwrap();
        assert_eq!(ok.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn completion_is_limited_to_50() {
        let entries: Vec<Value> = (0..120)
            .map(|i| json!({ "name": format!("f{i:03}"), "type": "blob" }))
            .collect();
        let b = Fake::new(json!({ "entries": entries }));
        let p = json!({
            "ref": { "type": "ref/resource", "uri": "asc://{owner}/{name}/blob/{rev}/{+path}" },
            "argument": { "name": "path", "value": "src/f" },
            "context": { "arguments": { "owner": "o", "name": "n", "rev": "main" } },
        });
        let v = rt(complete(&b, &p, 1_000_000, &CancellationToken::new())).unwrap();
        assert_eq!(
            v["completion"]["values"].as_array().unwrap().len(),
            MAX_COMPLETIONS
        );
        assert_eq!(v["completion"]["total"], 120);
        assert_eq!(v["completion"]["hasMore"], true);
        assert_eq!(v["completion"]["values"][0], "src/f000");
        assert_eq!(b.calls.lock().unwrap()[0].1["path"], "src");
    }

    #[test]
    fn completion_refuses_unsafe_input() {
        let b = Fake::new(json!({ "entries": [] }));
        let mk = |value: &str, rev: &str| {
            json!({
                "ref": { "type": "ref/resource", "uri": "x" },
                "argument": { "name": "path", "value": value },
                "context": { "arguments": { "rev": rev } },
            })
        };
        for (v, r) in [
            ("../x", "main"),
            ("a/../x", "main"),
            ("a/", "-x"),
            ("a\nb", "main"),
            ("a/", ""),
        ] {
            assert!(
                rt(complete(&b, &mk(v, r), 1000, &CancellationToken::new())).is_err(),
                "{v:?} {r:?}"
            );
        }
        assert!(b.calls.lock().unwrap().is_empty());
        let rev = json!({ "ref": { "type": "ref/resource", "uri": "x" }, "argument": { "name": "rev", "value": "HE" } });
        let v = rt(complete(&b, &rev, 1000, &CancellationToken::new())).unwrap();
        assert_eq!(v["completion"]["values"][0], "HEAD");
    }
}
