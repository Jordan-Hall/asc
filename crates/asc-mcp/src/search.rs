//! The `search` tool (SPEC 22.3.2 and 23.4.4, unit M15): the code-intelligence tool, listed only when
//! the server advertises `search.v1`.
//!
//! The tool is one catalogue row (`tools.toml`, one per surface). This module maps its arguments onto
//! the three `CodeIntel` RPCs by `mode` ([`plan`]), refuses combinations the row does not allow, and
//! shapes the reply so it always carries `scope`, `coverage` and `est_tokens` ([`finish`]). The
//! service itself sits behind [`CodeIntelClient`]; unit I21 wires the real one.
//!
//! Result text is untrusted file content: nothing here interprets it.

use std::collections::BTreeSet;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};

use crate::backend::{CallContext, McpBackend, Reply};
use crate::catalogue::{self, Context};
use crate::errors::ToolError;
use crate::shape::BYTES_PER_TOKEN;

/// The `Version.features` entry that lists the tool.
pub const FEATURE: &str = "search.v1";

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 200;
const DEFAULT_K: u64 = 10;
const MAX_K: u64 = 100;
const DEFAULT_CONTEXT: u64 = 2;
const MAX_CONTEXT: u64 = 20;
/// Default `max_tokens` of the row (still capped by `asc.mcp.maxTokens`).
const DEFAULT_MAX_TOKENS: u64 = 4_000;
const MAX_QUERY_BYTES: usize = 4096;
const MAX_PATHS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
const MAX_HANDLE_BYTES: usize = 256;

const ARGS: &[&str] = &[
    "repo",
    "query",
    "mode",
    "rev",
    "change",
    "stack",
    "diff_only",
    "paths",
    "limit",
    "op",
    "k",
    "context_lines",
    "max_tokens",
    "cursor",
    "rerank",
    "include_excluded",
];

/// The `CodeIntel` RPC a call maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rpc {
    /// `CodeIntel.Search` (server streaming): `mode` text or regex.
    Search,
    /// `CodeIntel.Symbols`: `mode` symbol.
    Symbols,
    /// `CodeIntel.Semantic`: `mode` semantic.
    Semantic,
}

impl Rpc {
    /// The method name on `asc.v1.CodeIntel`.
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Symbols => "Symbols",
            Self::Semantic => "Semantic",
        }
    }
}

/// One call mapped onto the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The RPC.
    pub rpc: Rpc,
    /// The request in the asc JSON profile, without `scope.repo`: the connection decides the
    /// repository, and a client adds it.
    pub request: Value,
    /// The `repo` argument (HTTP surface), for the client to check against its connection.
    pub repo: Option<String>,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new("INVALID_ARGUMENT", message)
}

fn text<'a>(args: &'a Map<String, Value>, key: &str, max: usize) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= max && !s.contains('\0') => Ok(Some(s)),
        Some(Value::String(_)) => Err(bad(format!("{key} is too long or contains NUL"))),
        Some(_) => Err(bad(format!("{key} must be a string"))),
    }
}

fn flag(args: &Map<String, Value>, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(format!("{key} must be a boolean"))),
    }
}

/// An integer argument, defaulted and clamped to `1..=max` (or `0..=max` when `zero_ok`).
fn number(args: &Map<String, Value>, key: &str, default: u64, max: u64, zero_ok: bool) -> Result<u64, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| bad(format!("{key} must be a non-negative integer")))?;
            if n == 0 && !zero_ok {
                return Err(bad(format!("{key} must be at least 1")));
            }
            Ok(n.min(max))
        }
    }
}

/// A scope path filter: relative, no `..`, no NUL, bounded. The server enforces ACLs; this keeps
/// obviously hostile filters off the wire.
fn clean_paths(args: &Map<String, Value>) -> Result<Vec<String>, ToolError> {
    let Some(v) = args.get("paths").filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
    let list = v.as_array().ok_or_else(|| bad("paths must be an array of strings"))?;
    if list.len() > MAX_PATHS {
        return Err(bad(format!("at most {MAX_PATHS} paths")));
    }
    let mut out = Vec::with_capacity(list.len());
    for p in list {
        let p = p.as_str().ok_or_else(|| bad("paths must be an array of strings"))?;
        out.push(safe_path(p)?);
    }
    Ok(out)
}

fn safe_path(p: &str) -> Result<String, ToolError> {
    let norm = p.replace('\\', "/");
    if p.is_empty()
        || p.len() > MAX_PATH_BYTES
        || p.contains('\0')
        || norm.starts_with('/')
        || norm.split('/').any(|c| c == "..")
        || norm.as_bytes().get(1) == Some(&b':')
    {
        return Err(ToolError::new(crate::errors::code::PATH_POLICY, "paths must be relative, without .. components"));
    }
    Ok(norm)
}

/// Maps validated tool arguments onto a `CodeIntel` request.
///
/// `budget_bytes` is the call's byte budget (`CallContext::budget_bytes`); the request's token
/// budget is the smaller of `max_tokens` and that budget, so `asc.mcp.maxTokens` always wins.
/// `page_token` is the verified cursor's token, if the call carried one.
///
/// # Errors
/// `INVALID_ARGUMENT` for an unknown field, mode or op, `op` outside `symbol` mode, more than one
/// of `rev`, `change` and `stack`, or an out-of-range value; `PATH_POLICY` for an unsafe path.
pub fn plan(args: &Value, budget_bytes: u64, page_token: Option<&str>) -> Result<Plan, ToolError> {
    let args = args.as_object().ok_or_else(|| bad("arguments must be an object"))?;
    if let Some(k) = args.keys().find(|k| !ARGS.contains(&k.as_str())) {
        return Err(bad(format!("unknown argument {:?}", k.chars().take(40).collect::<String>())));
    }
    let query = text(args, "query", MAX_QUERY_BYTES)?.filter(|q| !q.is_empty()).ok_or_else(|| bad("query is required"))?;
    let mode = text(args, "mode", 16)?.ok_or_else(|| bad("mode is required"))?;
    let op = text(args, "op", 16)?;
    if op.is_some() && mode != "symbol" {
        return Err(bad("op is only valid in symbol mode"));
    }
    if mode != "semantic" && (args.get("rerank").is_some_and(|v| !v.is_null()) || args.get("k").is_some_and(|v| !v.is_null())) {
        return Err(bad("k and rerank are only valid in semantic mode"));
    }

    let rev = text(args, "rev", MAX_HANDLE_BYTES)?;
    let change = text(args, "change", MAX_HANDLE_BYTES)?;
    let stack = text(args, "stack", MAX_HANDLE_BYTES)?;
    if [rev, change, stack].iter().filter(|x| x.is_some()).count() > 1 {
        return Err(bad("give at most one of rev, change and stack"));
    }
    let diff_only = flag(args, "diff_only")?;
    if diff_only && stack.is_none() {
        return Err(bad("diff_only needs stack"));
    }
    let mut scope = Map::new();
    if let Some(r) = rev {
        scope.insert("rev".into(), r.into());
    }
    if let Some(c) = change {
        scope.insert("change".into(), c.into());
    }
    if let Some(s) = stack {
        scope.insert("stack_top".into(), s.into());
    }
    if diff_only {
        scope.insert("stack_diff_only".into(), true.into());
    }
    let paths = clean_paths(args)?;
    if !paths.is_empty() {
        scope.insert("paths".into(), json!(paths));
    }
    // `workspace` scope is IPC only and has no argument here.
    if flag(args, "include_excluded")? {
        scope.insert("include_excluded".into(), true.into());
    }

    let limit = number(args, "limit", DEFAULT_LIMIT, MAX_LIMIT, false)?;
    let wanted = number(args, "max_tokens", DEFAULT_MAX_TOKENS, u64::MAX, false)?;
    let max_tokens = wanted.min((budget_bytes / BYTES_PER_TOKEN).max(1));
    let budget = json!({ "max_results": limit, "max_tokens": max_tokens });
    let context = number(args, "context_lines", DEFAULT_CONTEXT, MAX_CONTEXT, true)?;
    let repo = text(args, "repo", 256)?.map(str::to_owned);
    let scope = Value::Object(scope);

    let (rpc, request) = match mode {
        "text" | "regex" => {
            let mut r = json!({
                "scope": scope, "pattern": query, "mode": if mode == "regex" { "regex" } else { "literal" },
                "case_sensitive": true, "context_lines": context, "budget": budget,
            });
            if let Some(t) = page_token {
                r["page_token"] = t.into();
            }
            (Rpc::Search, r)
        }
        "symbol" => {
            let op = op.unwrap_or("def");
            if !["def", "refs", "callers", "outline", "find"].contains(&op) {
                return Err(bad("op must be def, refs, callers, outline or find"));
            }
            let mut r = json!({ "scope": scope, "op": op, "name": query, "budget": budget });
            if op == "outline" {
                // The outline of one file: the query is its path.
                r["name"] = "".into();
                r["path"] = safe_path(query)?.into();
            }
            if let Some(t) = page_token {
                r["page_token"] = t.into();
            }
            (Rpc::Symbols, r)
        }
        "semantic" => {
            let k = number(args, "k", DEFAULT_K, MAX_K, false)?;
            let rerank = flag(args, "rerank")?;
            (
                Rpc::Semantic,
                json!({ "scope": scope, "query": query, "mode": "hybrid", "k": k, "budget": budget, "rerank": rerank }),
            )
        }
        _ => return Err(bad("mode must be text, regex, symbol or semantic")),
    };
    Ok(Plan { rpc, request, repo })
}

/// Makes one reply message carry `scope`, `coverage` and `est_tokens` (SPEC 22.3.2). The service
/// fills `scope` and `est_tokens`; a message that lacks them gets `coverage` from `scope.coverage`
/// and `est_tokens` from its own size, so the contract holds with any backend.
#[must_use]
pub fn finish(mut message: Value) -> Value {
    let size = message.to_string().len() as u64;
    if let Value::Object(m) = &mut message {
        if !m.contains_key("coverage") {
            let cov = m.get("scope").and_then(|s| s.get("coverage")).cloned().unwrap_or_else(|| json!(0.0));
            m.insert("coverage".into(), cov);
        }
        if !m.contains_key("est_tokens") {
            m.insert("est_tokens".into(), json!(size.div_ceil(BYTES_PER_TOKEN)));
        }
    }
    message
}

/// The `CodeIntel` service as the tool sees it. The real client is wired by I21 (it adds the
/// repository from the connection, checks [`Plan::repo`] and sends the credential); tests use a fake.
#[async_trait::async_trait]
pub trait CodeIntelClient: Send + Sync + 'static {
    /// Runs `plan` and streams the response messages in the asc JSON profile.
    ///
    /// # Errors
    /// A [`ToolError`] for a failed call.
    async fn call(&self, plan: Plan, ctx: &CallContext) -> Result<BoxStream<'static, Result<Value, ToolError>>, ToolError>;
}

/// An [`McpBackend`] for the `search` tool over a [`CodeIntelClient`].
#[derive(Debug, Clone)]
pub struct SearchTool<C> {
    client: C,
}

impl<C: CodeIntelClient> SearchTool<C> {
    /// A backend that answers `search` through `client`.
    #[must_use]
    pub const fn new(client: C) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl<C: CodeIntelClient> McpBackend for SearchTool<C> {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        if tool != "search" {
            return Err(ToolError::not_available(format!("{tool} is not served here")));
        }
        let page = match &ctx.position {
            Some(crate::cursor::Position::Page { token }) => Some(token.as_str()),
            _ => None,
        };
        let plan = plan(args, ctx.budget_bytes, page)?;
        let stream = self.client.call(plan, ctx).await?;
        Ok(Reply::Messages(stream.map(|m| m.map(finish)).boxed()))
    }
}

/// Whether a change of `Version.features` from `before` to `after` changes `tools/list` under
/// `ctx`: `notifications/tools/list_changed` is due exactly then (for example `search.v1`
/// appearing once the service is enabled).
#[must_use]
pub fn list_changed(ctx: &Context, before: &BTreeSet<String>, after: &BTreeSet<String>) -> bool {
    let names = |features: &BTreeSet<String>| {
        let mut c = ctx.clone();
        c.features.clone_from(features);
        catalogue::tools(&c).iter().map(|l| l.def.name).collect::<Vec<_>>()
    };
    names(before) != names(after)
}

#[cfg(test)]
mod tests;
