//! The tools an agent run offers its model, and what each does.
//!
//! The model reads and searches the working copy and the code index, edits files, runs the
//! commands the operator allow-listed, looks at its diff and commits it as a change. There is no
//! shell and no network tool. Everything the model sends is untrusted: arguments are checked
//! against a schema, paths are confined to the working copy, and the result of a tool is bounded
//! and passed through the secret masker before the model or the event stream sees it.
//!
//! What a tool cannot do is more important than what it can:
//!
//! * No tool moves a ref other than the run's own `refs/heads/agent/<agent>/run-<id>`, and that one
//!   only through `Repo.Commit` with the run's token, so scope, budget and policy bind it.
//! * The budget and the scope are never read from anything the model or a file says; a file that
//!   tells the model to "raise the budget" changes nothing, because the numbers live in the
//!   token row.
//! * A tool error (a refused path, a failed command, a policy denial) is returned to the model as
//!   a result with `is_error` set; it is a normal event, not a failure of the run.

use std::sync::Arc;
use std::time::Duration;

use asc_ci::sandbox::{Sandbox, SandboxError, Spec};
use asc_rpc::server::wire;
use bytes::Bytes;
use regex::RegexBuilder;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tonic::Code;

use super::inproc::InProc;
use super::mask::{Masker, truncate};
use super::workspace::{Edit, Workspace};
use crate::ai::model::{ToolCall, ToolSpec};

/// Most bytes one tool result holds.
pub const MAX_RESULT: usize = 32 * 1024;
/// Most entries a listing returns.
const MAX_LIST: usize = 500;
/// Most matches a search returns.
const MAX_MATCHES: usize = 200;
/// Most files a local search reads.
const MAX_SEARCH_FILES: usize = 5000;
/// Largest file a local search reads.
const MAX_SEARCH_FILE: u64 = 1024 * 1024;
/// Longest commit message.
const MAX_MESSAGE: usize = 4096;
/// Most file operations in one commit.
const MAX_OPS: usize = crate::mutations::MAX_OPS;
/// Longest question to the person.
const MAX_QUESTION: usize = 4096;
/// Most scope refusals a run tolerates before it is stopped.
pub const MAX_SCOPE_REFUSALS: u32 = 5;

/// What a tool call came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Text for the model; `error` marks it as a failure the model should react to.
    Result { content: String, error: bool },
    /// The model asks the person a question and waits.
    Ask(String),
    /// The model says it is done.
    Finish(String),
    /// The run must stop: a limit the server enforces was passed.
    Stop(Stop),
}

/// Why a tool stopped the run.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// A budget (`changes`, `paths`, `tokens`, `cost`) ran out.
    Budget(String),
    /// Too many refusals to stay in scope.
    Scope,
}

fn ok(s: impl Into<String>) -> Outcome {
    Outcome::Result {
        content: s.into(),
        error: false,
    }
}

fn err(s: impl Into<String>) -> Outcome {
    Outcome::Result {
        content: s.into(),
        error: true,
    }
}

/// The tools offered, given whether commands are allowed.
pub fn specs(commands: &[String]) -> Vec<ToolSpec> {
    let mut v = vec![
        spec(
            "list_files",
            "List the entries of a directory of the working copy (the root when path is empty). With recursive, list every file below it.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "directory, relative to the repository root"},
                "recursive": {"type": "boolean"}}}),
        ),
        spec(
            "read_file",
            "Read a text file of the working copy. Use start_line and end_line (1-based, inclusive) to read part of a large file.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}, "start_line": {"type": "integer"}, "end_line": {"type": "integer"}},
                "required": ["path"]}),
        ),
        spec(
            "grep",
            "Search the text files of the working copy (including your edits) for a regular expression. Returns path:line: text.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string"}, "path": {"type": "string", "description": "limit to this directory or file"},
                "ignore_case": {"type": "boolean"}, "fixed": {"type": "boolean", "description": "treat the pattern as plain text"}},
                "required": ["pattern"]}),
        ),
        spec(
            "search_code",
            "Search the repository's code index at the commit this run started from. Faster than grep on a large repository; does not see your edits.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string"}, "mode": {"type": "string", "enum": ["literal", "regex", "word"]},
                "paths": {"type": "array", "items": {"type": "string"}}}, "required": ["pattern"]}),
        ),
        spec(
            "symbols",
            "Look up symbols in the code index: find by name, a definition, references, callers or the outline of a file.",
            json!({"type": "object", "properties": {
                "op": {"type": "string", "enum": ["find", "def", "refs", "callers", "outline"]},
                "name": {"type": "string"}, "path": {"type": "string"}}, "required": ["op"]}),
        ),
        spec(
            "write_file",
            "Create a file or replace it with new content.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"]}),
        ),
        spec(
            "edit_file",
            "Replace text in a file. old must occur exactly once unless replace_all is true.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"},
                "replace_all": {"type": "boolean"}}, "required": ["path", "old", "new"]}),
        ),
        spec(
            "delete_file",
            "Delete a file.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        ),
        spec(
            "move_file",
            "Move a file.",
            json!({"type": "object", "properties": {"from": {"type": "string"}, "to": {"type": "string"}},
                "required": ["from", "to"]}),
        ),
        spec(
            "status",
            "List the files you changed since your last commit (A added, M modified, D deleted).",
            json!({"type": "object", "properties": {}}),
        ),
        spec(
            "diff",
            "Show your uncommitted changes as a unified diff, for all files or one path.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ),
        spec(
            "commit",
            "Commit your uncommitted changes as one change on top of your earlier ones. Each commit is one reviewable step of a stack: keep them small and described. A commit the server refuses (outside your scope, over budget) comes back as an error.",
            json!({"type": "object", "properties": {"message": {"type": "string", "description": "the commit message: a short summary line, then optional details"}},
                "required": ["message"]}),
        ),
        spec(
            "ask_user",
            "Ask the person who started this run a question and wait for the answer. Use it when the task is ambiguous; do not guess.",
            json!({"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]}),
        ),
        spec(
            "finish",
            "End the run. Uncommitted changes are committed as a last change first. Give a short summary of what you did.",
            json!({"type": "object", "properties": {"summary": {"type": "string"}}, "required": ["summary"]}),
        ),
    ];
    if !commands.is_empty() {
        v.push(spec(
            "run_command",
            &format!(
                "Run a command in the working copy and return its exit code and output. There is no shell: give the program and its arguments as a list. Only these commands are allowed: {}.",
                commands.join("; ")
            ),
            json!({"type": "object", "properties": {
                "argv": {"type": "array", "items": {"type": "string"}},
                "timeout_seconds": {"type": "integer"}}, "required": ["argv"]}),
        ));
    }
    v
}

fn spec(name: &str, description: &str, schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        schema,
    }
}

/// Everything a tool call may touch.
pub struct ToolCtx<'a> {
    pub ws: &'a mut Workspace,
    pub sandbox: &'a Sandbox,
    pub max_command: Duration,
    pub masker: &'a Masker,
    pub inproc: Option<&'a InProc>,
    pub token: &'a str,
    pub repo: &'a str,
    pub agent: &'a str,
    pub run_id: u64,
    pub base_hex: &'a str,
    pub ref_name: &'a str,
    pub max_file: u64,
    pub cancel: &'a CancellationToken,
    /// The tip of the run's stack so far (a changeset id, 32 bytes), or the base.
    pub tip: &'a mut Vec<u8>,
    /// Whether the run has committed anything (the ref exists).
    pub ref_exists: &'a mut bool,
    /// Called with the change id and changeset after each commit.
    pub committed: &'a mut Vec<Committed>,
    pub scope_refusals: &'a mut u32,
    pub pool: &'a sqlx::AnyPool,
    pub ns: Vec<u8>,
}

/// One commit of the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    pub changeset: Vec<u8>,
    pub change: Vec<u8>,
    pub message: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn obj(call: &ToolCall) -> Result<&Map<String, Value>, Outcome> {
    call.input
        .as_object()
        .ok_or_else(|| err("the arguments must be a JSON object"))
}

fn text<'a>(m: &'a Map<String, Value>, k: &str) -> Result<&'a str, Outcome> {
    m.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| err(format!("{k} is required and must be a string")))
}

fn opt_text<'a>(m: &'a Map<String, Value>, k: &str) -> Result<Option<&'a str>, Outcome> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(err(format!("{k} must be a string"))),
    }
}

fn opt_bool(m: &Map<String, Value>, k: &str) -> Result<bool, Outcome> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(err(format!("{k} must be true or false"))),
    }
}

fn opt_uint(m: &Map<String, Value>, k: &str) -> Result<Option<u64>, Outcome> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| err(format!("{k} must be a non-negative integer"))),
    }
}

fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

impl ToolCtx<'_> {
    /// Runs one call. The result is already masked and bounded.
    pub async fn run(&mut self, call: &ToolCall) -> Outcome {
        let out = self.dispatch(call).await;
        match out {
            Outcome::Result { content, error } => Outcome::Result {
                content: truncate(self.masker.mask(&content), MAX_RESULT),
                error,
            },
            Outcome::Ask(q) => Outcome::Ask(self.masker.mask(&q)),
            Outcome::Finish(s) => Outcome::Finish(self.masker.mask(&s)),
            stop @ Outcome::Stop(_) => stop,
        }
    }

    async fn dispatch(&mut self, call: &ToolCall) -> Outcome {
        let m = match obj(call) {
            Ok(m) => m,
            Err(o) => return o,
        };
        let r = match call.name.as_str() {
            "list_files" => self.list_files(m).await,
            "read_file" => self.read_file(m).await,
            "grep" => self.grep(m).await,
            "search_code" => self.search_code(m).await,
            "symbols" => self.symbols(m).await,
            "write_file" => self.write_file(m).await,
            "edit_file" => self.edit_file(m).await,
            "delete_file" => self.delete_file(m).await,
            "move_file" => self.move_file(m).await,
            "status" => self.status().await,
            "diff" => self.diff(m).await,
            "commit" => self.commit(m).await,
            "ask_user" => {
                let q = text(m, "question");
                match q {
                    Ok(q) if !q.trim().is_empty() && q.len() <= MAX_QUESTION => {
                        Ok(Outcome::Ask(q.to_owned()))
                    }
                    Ok(_) => Err(err(format!("a question is 1 to {MAX_QUESTION} bytes"))),
                    Err(e) => Err(e),
                }
            }
            "finish" => text(m, "summary").map(|s| Outcome::Finish(s.chars().take(4096).collect())),
            "run_command" if self.sandbox.enabled() => self.run_command(m).await,
            other => Err(err(format!("there is no tool named {other:?}"))),
        };
        match r {
            Ok(o) | Err(o) => o,
        }
    }

    async fn list_files(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let path = opt_text(m, "path")?.unwrap_or("");
        let dir = if path.is_empty() {
            String::new()
        } else {
            super::workspace::clean_rel(path).map_err(|e| err(e.to_string()))?
        };
        if opt_bool(m, "recursive")? {
            let files = self.ws.files().await.map_err(|e| err(e.to_string()))?;
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let mut lines: Vec<String> = files
                .iter()
                .filter(|(p, _)| p.starts_with(&prefix))
                .map(|(p, s)| format!("{p} ({s} bytes)"))
                .collect();
            let total = lines.len();
            lines.truncate(MAX_LIST);
            let mut out = lines.join("\n");
            if total > MAX_LIST {
                out.push_str(&format!("\n[... {} more]", total - MAX_LIST));
            }
            return Ok(ok(if out.is_empty() {
                "(no files)".to_owned()
            } else {
                out
            }));
        }
        let entries = self.ws.list(&dir).await.map_err(|e| err(e.to_string()))?;
        let mut lines: Vec<String> = entries
            .iter()
            .map(|(n, d, s)| {
                if *d {
                    format!("{n}/")
                } else {
                    format!("{n} ({s} bytes)")
                }
            })
            .collect();
        let total = lines.len();
        lines.truncate(MAX_LIST);
        let mut out = lines.join("\n");
        if total > MAX_LIST {
            out.push_str(&format!("\n[... {} more]", total - MAX_LIST));
        }
        Ok(ok(if out.is_empty() {
            "(empty)".to_owned()
        } else {
            out
        }))
    }

    async fn read_file(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let path = text(m, "path")?;
        let data = self
            .ws
            .read(path, self.max_file)
            .await
            .map_err(|e| err(e.to_string()))?;
        if is_binary(&data) {
            return Err(err(format!("{path} is a binary file")));
        }
        let s = String::from_utf8_lossy(&data).into_owned();
        let (start, end) = (opt_uint(m, "start_line")?, opt_uint(m, "end_line")?);
        let body = if start.is_some() || end.is_some() {
            let first = usize::try_from(start.unwrap_or(1).max(1)).unwrap_or(usize::MAX);
            let last = usize::try_from(end.unwrap_or(u64::MAX)).unwrap_or(usize::MAX);
            if last < first {
                return Err(err("end_line is before start_line"));
            }
            s.lines()
                .enumerate()
                .filter(|(i, _)| (first..=last).contains(&(i + 1)))
                .map(|(_, l)| l)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            s
        };
        if body.len() > MAX_RESULT {
            return Ok(ok(format!(
                "{}\n[... the file is larger than {MAX_RESULT} bytes; read it in parts with start_line and end_line]",
                truncate(body, MAX_RESULT - 128)
            )));
        }
        Ok(ok(body))
    }

    async fn grep(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let pattern = text(m, "pattern")?;
        if pattern.is_empty() || pattern.len() > 1024 {
            return Err(err("a pattern is 1 to 1024 bytes"));
        }
        let scope = match opt_text(m, "path")? {
            Some(p) if !p.is_empty() => {
                Some(super::workspace::clean_rel(p).map_err(|e| err(e.to_string()))?)
            }
            _ => None,
        };
        let src = if opt_bool(m, "fixed")? {
            regex::escape(pattern)
        } else {
            pattern.to_owned()
        };
        let re = RegexBuilder::new(&src)
            .case_insensitive(opt_bool(m, "ignore_case")?)
            .size_limit(1 << 20)
            .dfa_size_limit(1 << 20)
            .build()
            .map_err(|_| err("the pattern is not a valid regular expression"))?;
        let files = self.ws.files().await.map_err(|e| err(e.to_string()))?;
        let mut out = Vec::new();
        let mut scanned = 0usize;
        'files: for (path, size) in files {
            if let Some(s) = &scope
                && path != *s
                && !path.starts_with(&format!("{s}/"))
            {
                continue;
            }
            if size > MAX_SEARCH_FILE {
                continue;
            }
            scanned += 1;
            if scanned > MAX_SEARCH_FILES {
                out.push("[... stopped after searching 5000 files]".to_owned());
                break;
            }
            let Ok(data) = self.ws.read(&path, MAX_SEARCH_FILE).await else {
                continue;
            };
            if is_binary(&data) {
                continue;
            }
            for (i, line) in String::from_utf8_lossy(&data).lines().enumerate() {
                if re.is_match(line) {
                    let shown: String = line.chars().take(300).collect();
                    out.push(format!("{path}:{}: {shown}", i + 1));
                    if out.len() >= MAX_MATCHES {
                        out.push(format!("[... stopped after {MAX_MATCHES} matches]"));
                        break 'files;
                    }
                }
            }
        }
        Ok(ok(if out.is_empty() {
            "no matches".to_owned()
        } else {
            out.join("\n")
        }))
    }

    fn scope(&self) -> wire::Scope {
        wire::Scope {
            repo: Some(wire::RepoRef {
                name: self.repo.to_owned(),
                id_mode: asc_rpc::IdMode::Asc as i32,
            }),
            at: Some(wire::scope::At::Rev(self.base_hex.to_owned())),
            ..wire::Scope::default()
        }
    }

    fn intel_error(e: &tonic::Status) -> Outcome {
        match e.code() {
            Code::Unimplemented | Code::FailedPrecondition | Code::NotFound | Code::Unavailable => {
                err("the code index is not available for this repository; use grep")
            }
            _ => err(format!(
                "the code index refused the request: {}",
                e.message()
            )),
        }
    }

    async fn search_code(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let pattern = text(m, "pattern")?;
        if pattern.is_empty() || pattern.len() > 1024 {
            return Err(err("a pattern is 1 to 1024 bytes"));
        }
        let mode = match opt_text(m, "mode")?.unwrap_or("literal") {
            "literal" => wire::SearchMode::Literal,
            "regex" => wire::SearchMode::Regex,
            "word" => wire::SearchMode::Word,
            _ => return Err(err("mode is literal, regex or word")),
        };
        let Some(inproc) = self.inproc else {
            return Err(err(
                "the code index is not available for this repository; use grep",
            ));
        };
        if self.base_hex.is_empty() {
            return Ok(ok("the repository is empty"));
        }
        let mut scope = self.scope();
        if let Some(Value::Array(ps)) = m.get("paths") {
            scope.paths = ps
                .iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .take(16)
                .collect();
        }
        let req = wire::SearchRequest {
            scope: Some(scope),
            pattern: pattern.to_owned(),
            mode: mode as i32,
            case_sensitive: true,
            context_lines: 0,
            budget: Some(wire::Budget {
                max_results: 60,
                max_bytes: 24 * 1024,
                max_tokens: 0,
            }),
            page_token: String::new(),
        };
        let pages = inproc
            .search(self.token, req)
            .await
            .map_err(|e| Self::intel_error(&e))?;
        let mut lines = Vec::new();
        for p in pages {
            for f in p.files {
                let path = String::from_utf8_lossy(&f.path).into_owned();
                for lm in f.matches {
                    let t = String::from_utf8_lossy(&lm.text);
                    let shown: String = t.chars().take(300).collect();
                    lines.push(format!("{path}:{}: {shown}", lm.line));
                }
            }
        }
        Ok(ok(if lines.is_empty() {
            "no matches".to_owned()
        } else {
            lines.join("\n")
        }))
    }

    async fn symbols(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let op = match text(m, "op")? {
            "find" => wire::SymbolOp::Find,
            "def" => wire::SymbolOp::Def,
            "refs" => wire::SymbolOp::Refs,
            "callers" => wire::SymbolOp::Callers,
            "outline" => wire::SymbolOp::Outline,
            _ => return Err(err("op is find, def, refs, callers or outline")),
        };
        let name = opt_text(m, "name")?.unwrap_or("");
        let path = opt_text(m, "path")?.unwrap_or("");
        if name.len() > 512 || path.len() > 1024 {
            return Err(err("name or path is too long"));
        }
        let Some(inproc) = self.inproc else {
            return Err(err(
                "the code index is not available for this repository; use grep",
            ));
        };
        if self.base_hex.is_empty() {
            return Ok(ok("the repository is empty"));
        }
        let req = wire::SymbolsRequest {
            scope: Some(self.scope()),
            op: op as i32,
            name: name.to_owned(),
            path: Bytes::copy_from_slice(path.as_bytes()),
            budget: Some(wire::Budget {
                max_results: 60,
                max_bytes: 24 * 1024,
                max_tokens: 0,
            }),
            ..wire::SymbolsRequest::default()
        };
        let r = inproc
            .symbols(self.token, req)
            .await
            .map_err(|e| Self::intel_error(&e))?;
        let lines: Vec<String> = r
            .symbols
            .iter()
            .map(|s| {
                format!(
                    "{} {} {}:{}{}",
                    s.kind,
                    s.name,
                    String::from_utf8_lossy(&s.path),
                    s.line,
                    if s.is_def { " (definition)" } else { "" }
                )
            })
            .collect();
        Ok(ok(if lines.is_empty() {
            "no symbols".to_owned()
        } else {
            lines.join("\n")
        }))
    }

    async fn write_file(&mut self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let path = text(m, "path")?;
        let content = text(m, "content")?;
        self.ws
            .write(path, content.as_bytes())
            .await
            .map_err(|e| err(e.to_string()))?;
        Ok(ok(format!("wrote {path} ({} bytes)", content.len())))
    }

    async fn edit_file(&mut self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let path = text(m, "path")?;
        let old = text(m, "old")?;
        let new = text(m, "new")?;
        if old.is_empty() {
            return Err(err(
                "old must not be empty; use write_file to create a file",
            ));
        }
        let data = self
            .ws
            .read(path, self.max_file)
            .await
            .map_err(|e| err(e.to_string()))?;
        if is_binary(&data) {
            return Err(err(format!("{path} is a binary file")));
        }
        let s = String::from_utf8(data).map_err(|_| err(format!("{path} is not UTF-8 text")))?;
        let n = s.matches(old).count();
        let all = opt_bool(m, "replace_all")?;
        if n == 0 {
            return Err(err(format!("old does not occur in {path}")));
        }
        if n > 1 && !all {
            return Err(err(format!(
                "old occurs {n} times in {path}; add context to make it unique, or set replace_all"
            )));
        }
        let updated = if all {
            s.replace(old, new)
        } else {
            s.replacen(old, new, 1)
        };
        self.ws
            .write(path, updated.as_bytes())
            .await
            .map_err(|e| err(e.to_string()))?;
        Ok(ok(format!(
            "edited {path} ({n} replacement{})",
            if n == 1 { "" } else { "s" }
        )))
    }

    async fn delete_file(&mut self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let path = text(m, "path")?;
        self.ws.delete(path).await.map_err(|e| err(e.to_string()))?;
        Ok(ok(format!("deleted {path}")))
    }

    async fn move_file(&mut self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let (from, to) = (text(m, "from")?, text(m, "to")?);
        self.ws
            .rename(from, to)
            .await
            .map_err(|e| err(e.to_string()))?;
        Ok(ok(format!("moved {from} to {to}")))
    }

    async fn status(&self) -> Result<Outcome, Outcome> {
        let lines = self.ws.status().await.map_err(|e| err(e.to_string()))?;
        Ok(ok(if lines.is_empty() {
            "no uncommitted changes".to_owned()
        } else {
            lines.join("\n")
        }))
    }

    async fn diff(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let only = match opt_text(m, "path")? {
            Some(p) if !p.is_empty() => {
                Some(super::workspace::clean_rel(p).map_err(|e| err(e.to_string()))?)
            }
            _ => None,
        };
        let d = self
            .ws
            .diff(only.as_deref(), MAX_RESULT)
            .await
            .map_err(|e| err(e.to_string()))?;
        Ok(ok(if d.is_empty() {
            "no uncommitted changes".to_owned()
        } else {
            d
        }))
    }

    async fn run_command(&self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let argv: Vec<String> = match m.get("argv") {
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| err("argv must be a list of strings"))
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(err("argv is required and must be a list of strings")),
        };
        let wanted = opt_uint(m, "timeout_seconds")?.unwrap_or(600);
        let timeout = Duration::from_secs(wanted.clamp(1, self.max_command.as_secs().max(1)));
        let spec = Spec {
            argv,
            cwd: self.ws.tree().to_path_buf(),
            root: self.ws.dir().to_path_buf(),
            env: Default::default(),
            secrets: Vec::new(),
            timeout,
        };
        match self.sandbox.run(&spec, self.cancel).await {
            Ok(o) => {
                let head = if o.timed_out {
                    format!("the command timed out after {} seconds", timeout.as_secs())
                } else if o.cancelled {
                    "the command was cancelled".to_owned()
                } else {
                    format!(
                        "exit code {}",
                        o.code
                            .map_or_else(|| "none (killed)".to_owned(), |c| c.to_string())
                    )
                };
                let tail = if o.truncated {
                    "\n[output was cut]"
                } else {
                    ""
                };
                Ok(Outcome::Result {
                    content: format!("{head}\n{}{tail}", o.output),
                    error: !o.success(),
                })
            }
            Err(e @ (SandboxError::NotAllowed | SandboxError::NotBare | SandboxError::Empty)) => {
                Err(err(format!(
                    "{e}; allowed: {}",
                    self.sandbox.allowed_text().join("; ")
                )))
            }
            Err(SandboxError::NotIsolated | SandboxError::Disabled) => {
                Err(err("commands cannot run on this server"))
            }
            Err(e) => Err(err(e.to_string())),
        }
    }

    async fn commit(&mut self, m: &Map<String, Value>) -> Result<Outcome, Outcome> {
        let message = text(m, "message")?.trim().to_owned();
        if message.is_empty() || message.len() > MAX_MESSAGE {
            return Err(err(format!("a commit message is 1 to {MAX_MESSAGE} bytes")));
        }
        self.commit_message(&message).await
    }

    /// Commits the uncommitted changes with `message`; used by the `commit` tool and by the final
    /// commit of a run.
    pub async fn commit_message(&mut self, message: &str) -> Result<Outcome, Outcome> {
        let Some(inproc) = self.inproc else {
            return Err(err("committing is not available"));
        };
        let edits = self
            .ws
            .edits(MAX_OPS)
            .await
            .map_err(|e| err(e.to_string()))?;
        if edits.is_empty() {
            return Err(err("there is nothing to commit"));
        }
        let ops: Vec<wire::FileOp> = edits
            .iter()
            .map(|e| match e {
                Edit::Put {
                    path,
                    content,
                    exec,
                } => wire::FileOp {
                    path: Bytes::copy_from_slice(path.as_bytes()),
                    op: Some(wire::file_op::Op::PutContent(content.clone())),
                    mode: if *exec { 0o100_755 } else { 0o100_644 },
                },
                Edit::Delete { path } => wire::FileOp {
                    path: Bytes::copy_from_slice(path.as_bytes()),
                    op: Some(wire::file_op::Op::Delete(true)),
                    mode: 0,
                },
            })
            .collect();
        let now = crate::ai::clock::now();
        let sig = wire::Signature {
            name: Bytes::copy_from_slice(format!("{} (agent)", self.agent).as_bytes()),
            email: Bytes::copy_from_slice(format!("{}@agents.invalid", self.agent).as_bytes()),
            time: now,
            tz_min: 0,
        };
        let expect = (*self.ref_exists).then(|| wire::RefTarget {
            t: Some(wire::ref_target::T::Changeset(Bytes::copy_from_slice(
                self.tip,
            ))),
        });
        let req = wire::CommitRequest {
            repo: Some(wire::RepoRef {
                name: self.repo.to_owned(),
                id_mode: asc_rpc::IdMode::Asc as i32,
            }),
            parent: if self.tip.is_empty() {
                String::new()
            } else {
                hex(self.tip)
            },
            ops,
            message: Bytes::copy_from_slice(message.as_bytes()),
            author: Some(sig.clone()),
            committer: Some(sig),
            update_ref: self.ref_name.to_owned(),
            expect,
            sign: false,
        };
        let resp = match inproc.commit(self.token, req).await {
            Ok(r) => r,
            Err(status) => return Ok(self.refused(&status)),
        };
        let changeset = resp.id.to_vec();
        *self.tip = changeset.clone();
        *self.ref_exists = true;
        let change: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
                .bind(self.ns.clone())
                .bind(changeset.clone())
                .fetch_optional(self.pool)
                .await
                .ok()
                .flatten();
        let change = change.unwrap_or_default();
        // The next diff is against what was just committed.
        let new_root = self.ws.root_of_commit(&changeset).await;
        if let Some(root) = new_root {
            let _ = self.ws.rebase_to(root).await;
        }
        self.committed.push(Committed {
            changeset: changeset.clone(),
            change: change.clone(),
            message: message.to_owned(),
        });
        Ok(ok(format!(
            "committed change {} (changeset {}), {} file{}",
            if change.is_empty() {
                "?".to_owned()
            } else {
                hex(&change)
            },
            &hex(&changeset)[..12.min(changeset.len() * 2)],
            edits.len(),
            if edits.len() == 1 { "" } else { "s" }
        )))
    }

    /// A refused call as a tool result, or the stop it forces.
    fn refused(&mut self, status: &tonic::Status) -> Outcome {
        let detail = asc_rpc::server::status_rpc_error(status);
        let code = detail
            .as_ref()
            .map(|d| d.code().to_owned())
            .unwrap_or_default();
        let paths = detail
            .as_ref()
            .map(|d| d.info().paths.clone())
            .unwrap_or_default();
        match code.as_str() {
            "AGENT_SCOPE_DENIED" => {
                *self.scope_refusals += 1;
                if *self.scope_refusals >= MAX_SCOPE_REFUSALS {
                    return Outcome::Stop(Stop::Scope);
                }
                err(format!(
                    "refused: these paths are outside what you may change: {}. Remove those changes and commit again.",
                    paths.join(", ")
                ))
            }
            "BUDGET_EXCEEDED" => {
                let dim = paths
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "changes".to_owned());
                match dim.as_str() {
                    "tokens" | "cost" => Outcome::Stop(Stop::Budget(dim)),
                    _ => err(format!(
                        "refused: the {dim} budget of this run is used up; finish now."
                    )),
                }
            }
            "POLICY_DENIED" | "SECRET_DETECTED" | "PATH_POLICY" | "AGENTS_DISABLED"
            | "HOOK_REJECTED" => err(format!(
                "refused by the repository ({code}): {}",
                if paths.is_empty() {
                    status.message().to_owned()
                } else {
                    paths.join("; ")
                }
            )),
            _ => err(format!(
                "the commit failed ({:?}): {}",
                status.code(),
                status.message()
            )),
        }
    }
}

/// Arc helper so callers need not name the type.
pub type SharedMasker = Arc<Masker>;
