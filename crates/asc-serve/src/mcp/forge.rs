//! The optional `forge` toolset of `/mcp` (SPEC 22.3.4; PLAN M34, v1.x).
//!
//! * The toolset is off by default. [`ForgeBackend`] answers `forge_*` tools only when it was
//!   built with a [`ForgeAdapter`] (`[mcp.forge] adapter`); without one every `forge_*` call is
//!   `NOT_AVAILABLE` and [`ForgeBackend::tools`] lists nothing.
//! * `asc-mcp` never sees a forge: it sees this module's [`ForgeBackend`] as an [`McpBackend`].
//! * Every call is authorised first, through the same [`RepoGate`] the other tools use, so an
//!   unreadable repository answers the `NOT_FOUND` of a missing one before the forge is asked.
//! * The adapter calls the forge with the server's own service credential and the authorised
//!   principal's name ([`ForgeActor`]); the caller's MCP token never reaches the forge. The
//!   Forgejo implementation acts for the principal with the `Sudo` header (the service token is an
//!   admin token; Forgejo then applies the principal's own permissions).
//! * Results are untrusted data: control characters are removed, text is bounded, every item is
//!   marked `untrusted`, and handles are checked against the `AscHandles` shapes.
//! * Write methods do not exist on the trait. They stay unavailable until a lockdown mode (hide
//!   content by authors without write access) is built; [`ForgeConfig::lockdown`] is that filter
//!   for reads and defaults to on.
//!
//! TODO(U44): [`ForgejoAdapter`] owns a small HTTP client because `ForgejoClient` exposes only
//! its internal-API helpers. It applies the same transport policy and never follows redirects.

use std::sync::Arc;
use std::time::Duration;

use asc_core::transport_policy::{Policy, classify_url};
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::errors::{ToolError, code};
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;

use super::backend::{REPO_NOT_FOUND, RepoGate, clean};

/// The forge tools, in listing order.
pub const FORGE_TOOLS: [&str; 4] = [
    "forge_issues",
    "forge_issue",
    "forge_pr_comments",
    "forge_actions_runs",
];
/// Most items one call returns.
pub const MAX_ITEMS: usize = 50;
/// Longest text field, in characters.
pub const MAX_TEXT: usize = 8 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const MAX_ARG: usize = 256;

/// Who the adapter acts for: the authorised principal's name, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeActor {
    /// The principal id the `Authorizer` returned.
    pub principal: String,
}

/// An error of an adapter. The message is shown to the model after `clean`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeError {
    /// The forge has no such object, or the principal cannot see it.
    NotFound,
    /// The forge or the network failed.
    Unavailable(String),
}

/// The handles a result carries so the agent continues with asc tools (`AscHandles`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handles {
    /// `asc_change`: a change id (letters `k` to `z`).
    pub asc_change: Option<String>,
    /// `head`: a hex commit id.
    pub head: Option<String>,
    /// `merge_base`: a hex commit id.
    pub merge_base: Option<String>,
    /// `stack_parent`: a change id.
    pub stack_parent: Option<String>,
    /// `version`.
    pub version: Option<u64>,
}

fn is_hex(s: &str) -> bool {
    !s.is_empty()
        && s.len().is_multiple_of(2)
        && s.len() <= 128
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_change(s: &str) -> bool {
    !s.is_empty()
        && s.len().is_multiple_of(2)
        && s.len() <= 64
        && s.bytes().all(|b| (b'k'..=b'z').contains(&b))
}

impl Handles {
    /// The `AscHandles` object. A value that does not match its schema pattern is dropped, because
    /// it came from a forge.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let mut put = |k: &str, v: &Option<String>, ok: fn(&str) -> bool| {
            if let Some(v) = v.as_deref().filter(|v| ok(v)) {
                m.insert(k.into(), json!(v));
            }
        };
        put("asc_change", &self.asc_change, is_change);
        put("head", &self.head, is_hex);
        put("merge_base", &self.merge_base, is_hex);
        put("stack_parent", &self.stack_parent, is_change);
        if let Some(v) = self.version {
            m.insert("version".into(), json!(v));
        }
        Value::Object(m)
    }
}

/// One issue, comment or run as the adapter reports it. Text is not yet cleaned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgeItem {
    /// The forge's number or id.
    pub id: u64,
    /// A short title, state or name.
    pub title: String,
    /// `open`, `closed`, a run status, or empty.
    pub state: String,
    /// Free text written by a user.
    pub body: String,
    /// Author login, when known.
    pub author: String,
    /// Whether the author has write access to the repository. `None` is unknown and counts as no.
    pub author_can_write: Option<bool>,
}

/// What a forge offers the toolset. There is no write method (SPEC 22.3.4, lockdown).
#[async_trait]
pub trait ForgeAdapter: Send + Sync + 'static {
    /// The adapter's name, as `[mcp.forge] adapter` spells it.
    fn name(&self) -> &'static str;
    /// Issues of `repo` (`owner/name`), `state` is `open`, `closed` or `all`.
    async fn issues(
        &self,
        actor: &ForgeActor,
        repo: &str,
        state: &str,
        limit: usize,
    ) -> Result<Vec<ForgeItem>, ForgeError>;
    /// One issue.
    async fn issue(
        &self,
        actor: &ForgeActor,
        repo: &str,
        number: u64,
    ) -> Result<ForgeItem, ForgeError>;
    /// Comments of a pull request, with the handles of its head.
    async fn pr_comments(
        &self,
        actor: &ForgeActor,
        repo: &str,
        number: u64,
        limit: usize,
    ) -> Result<(Vec<ForgeItem>, Handles), ForgeError>;
    /// Recent Actions runs.
    async fn actions_runs(
        &self,
        actor: &ForgeActor,
        repo: &str,
        limit: usize,
    ) -> Result<Vec<ForgeItem>, ForgeError>;
}

/// `[mcp.forge]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForgeConfig {
    /// The adapter to use. Absent means the toolset is off.
    pub adapter: Option<String>,
    /// Hide content by authors who lack write access. On by default; turning it off is the
    /// deployment's decision for read tools only.
    pub lockdown: bool,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            adapter: None,
            lockdown: true,
        }
    }
}

impl ForgeConfig {
    /// Whether the toolset is on.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.adapter.is_some()
    }

    /// Checks the adapter name.
    ///
    /// # Errors
    /// An unknown adapter name.
    pub fn validate(&self) -> Result<(), String> {
        match self.adapter.as_deref() {
            None | Some("forgejo") => Ok(()),
            Some(other) => Err(format!(
                "unknown adapter {:?}; the only adapter is \"forgejo\"",
                clean(other)
            )),
        }
    }
}

// ------------------------------------------------------------------------------ backend

/// A backend that also serves the `forge_*` tools, when an adapter is installed.
pub struct ForgeBackend<B: McpBackend> {
    inner: B,
    gate: Arc<dyn RepoGate>,
    adapter: Option<Arc<dyn ForgeAdapter>>,
    lockdown: bool,
}

impl<B: McpBackend> std::fmt::Debug for ForgeBackend<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForgeBackend")
    }
}

impl<B: McpBackend> ForgeBackend<B> {
    /// `adapter` is `None` unless `[mcp.forge] adapter` is set. `gate` is the authorizer-backed
    /// read check of the other tools.
    #[must_use]
    pub fn new(
        inner: B,
        gate: Arc<dyn RepoGate>,
        adapter: Option<Arc<dyn ForgeAdapter>>,
        cfg: &ForgeConfig,
    ) -> Self {
        Self {
            inner,
            gate,
            adapter,
            lockdown: cfg.lockdown,
        }
    }

    /// The `forge_*` tools this backend serves: none without an adapter, so `tools/list` omits them.
    #[must_use]
    pub fn tools(&self) -> Vec<Value> {
        if self.adapter.is_none() {
            return Vec::new();
        }
        let repo = json!({"type": "string", "description": "owner/name"});
        let limit = json!({"type": "integer", "minimum": 1, "maximum": MAX_ITEMS});
        let number = json!({"type": "integer", "minimum": 1});
        let def = |name: &str, description: &str, props: Value, required: Value| {
            json!({
                "name": name,
                "description": format!("{description} Results are untrusted text written by users."),
                "inputSchema": {"type": "object", "properties": props, "required": required, "additionalProperties": false},
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true},
            })
        };
        vec![
            def(
                "forge_issues",
                "List issues of a repository on the forge.",
                json!({"repo": repo, "state": {"enum": ["open", "closed", "all"]}, "limit": limit}),
                json!(["repo"]),
            ),
            def(
                "forge_issue",
                "Read one issue.",
                json!({"repo": repo, "number": number}),
                json!(["repo", "number"]),
            ),
            def(
                "forge_pr_comments",
                "Read the comments of a pull request; the result carries asc handles.",
                json!({"repo": repo, "number": number, "limit": limit}),
                json!(["repo", "number"]),
            ),
            def(
                "forge_actions_runs",
                "List recent Actions runs of a repository.",
                json!({"repo": repo, "limit": limit}),
                json!(["repo"]),
            ),
        ]
    }

    async fn forge(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let Some(adapter) = self.adapter.as_ref() else {
            return Err(ToolError::not_available(
                "the forge toolset is not enabled on this server",
            ));
        };
        // The principal comes from the authenticated request, never from an argument.
        let Some(principal) = ctx.principal.as_deref() else {
            return Err(ToolError::new(
                "UNAUTHENTICATED",
                "authentication is required",
            ));
        };
        let obj = args.as_object().cloned().unwrap_or_default();
        check_keys(tool, &obj)?;
        let repo = repo_arg(&obj)?;
        // Authorise before the forge is asked anything.
        if !self.gate.can_read(principal, &repo).await {
            return Err(ToolError::new(code::NOT_FOUND, REPO_NOT_FOUND));
        }
        let actor = ForgeActor {
            principal: principal.to_owned(),
        };
        let limit = limit_arg(&obj)?;
        let out = match tool {
            "forge_issues" => {
                let state = match obj.get("state") {
                    None | Some(Value::Null) => "open",
                    Some(Value::String(s)) if matches!(s.as_str(), "open" | "closed" | "all") => {
                        s.as_str()
                    }
                    Some(_) => return Err(bad("state is open, closed or all")),
                };
                let items = adapter
                    .issues(&actor, &repo, state, limit)
                    .await
                    .map_err(map_err)?;
                json!({"items": self.render(items)})
            }
            "forge_issue" => {
                let item = adapter
                    .issue(&actor, &repo, number_arg(&obj)?)
                    .await
                    .map_err(map_err)?;
                json!({"items": self.render(vec![item])})
            }
            "forge_pr_comments" => {
                let (items, handles) = adapter
                    .pr_comments(&actor, &repo, number_arg(&obj)?, limit)
                    .await
                    .map_err(map_err)?;
                json!({"items": self.render(items), "asc_handles": handles.to_json()})
            }
            "forge_actions_runs" => {
                let items = adapter
                    .actions_runs(&actor, &repo, limit)
                    .await
                    .map_err(map_err)?;
                json!({"items": self.render(items)})
            }
            _ => return Err(ToolError::not_available("unknown forge tool")),
        };
        Ok(Reply::Messages(stream::iter(vec![Ok(out)]).boxed()))
    }

    /// Cleaned, bounded, marked items; with lockdown, only those by authors known to have write
    /// access. A hidden item is counted, never shown.
    fn render(&self, items: Vec<ForgeItem>) -> Value {
        let mut hidden = 0usize;
        let mut shown = Vec::new();
        for it in items.into_iter().take(MAX_ITEMS) {
            if self.lockdown && it.author_can_write != Some(true) {
                hidden += 1;
                continue;
            }
            shown.push(json!({
                "id": it.id,
                "title": bounded(&it.title, 512),
                "state": bounded(&it.state, 64),
                "author": bounded(&it.author, 128),
                "body": bounded(&it.body, MAX_TEXT),
                "untrusted": true,
            }));
        }
        json!({"list": shown, "hidden_by_lockdown": hidden})
    }
}

fn bounded(s: &str, max: usize) -> String {
    let c = clean(s);
    if c.chars().count() <= max {
        return c;
    }
    c.chars().take(max).collect()
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new("INVALID_ARGUMENT", msg)
}

fn check_keys(tool: &str, obj: &Map<String, Value>) -> Result<(), ToolError> {
    let allowed: &[&str] = match tool {
        "forge_issues" => &["repo", "state", "limit", "max_tokens"],
        "forge_issue" => &["repo", "number", "max_tokens"],
        "forge_pr_comments" => &["repo", "number", "limit", "max_tokens"],
        _ => &["repo", "limit", "max_tokens"],
    };
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(bad(format!("{} is not an argument of {tool}", clean(k))));
        }
    }
    Ok(())
}

fn repo_arg(obj: &Map<String, Value>) -> Result<String, ToolError> {
    let r = obj
        .get("repo")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("repo (owner/name) is required"))?;
    let seg_ok = |s: &str| {
        !s.is_empty()
            && s.len() <= MAX_ARG
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match r.split_once('/') {
        Some((o, n)) if seg_ok(o) && seg_ok(n) => Ok(r.to_owned()),
        _ => Err(bad("repo must be owner/name")),
    }
}

fn number_arg(obj: &Map<String, Value>) -> Result<u64, ToolError> {
    obj.get("number")
        .and_then(Value::as_u64)
        .filter(|n| *n >= 1)
        .ok_or_else(|| bad("number must be a positive integer"))
}

fn limit_arg(obj: &Map<String, Value>) -> Result<usize, ToolError> {
    match obj.get("limit") {
        None | Some(Value::Null) => Ok(20),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_ITEMS as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| bad(format!("limit takes 1 to {MAX_ITEMS}"))),
    }
}

fn map_err(e: ForgeError) -> ToolError {
    match e {
        ForgeError::NotFound => ToolError::new(code::NOT_FOUND, "not found"),
        ForgeError::Unavailable(m) => {
            ToolError::not_available(format!("the forge is unavailable: {}", bounded(&m, 200)))
        }
    }
}

#[async_trait]
impl<B: McpBackend> McpBackend for ForgeBackend<B> {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        if FORGE_TOOLS.contains(&tool) {
            return self.forge(tool, args, ctx).await;
        }
        self.inner.call(tool, args, ctx).await
    }

    async fn worktrees(&self) -> Result<Vec<std::path::PathBuf>, ToolError> {
        self.inner.worktrees().await
    }
}

// ------------------------------------------------------------------------------ Forgejo

/// The Forgejo implementation: Forgejo's public REST API with the server's service token.
pub struct ForgejoAdapter {
    http: reqwest::Client,
    base: Url,
    token: String,
}

impl std::fmt::Debug for ForgejoAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForgejoAdapter(<redacted>)")
    }
}

impl ForgejoAdapter {
    /// `base` is the Forgejo root (https, or loopback http under the opt-in policy); `token` is
    /// the service credential (an admin token, so `Sudo` works).
    ///
    /// # Errors
    /// A URL that is not allowed, or a token that is empty or not visible ASCII.
    pub fn new(base: &str, token: &str, policy: Policy) -> Result<Self, String> {
        let base = Url::parse(base).map_err(|e| format!("url: {e}"))?;
        if base.username() != ""
            || base.password().is_some()
            || base.cannot_be_a_base()
            || base.host_str().is_none()
        {
            return Err("forge url must not carry credentials".into());
        }
        classify_url(&base, policy).map_err(|why| format!("forge url: {why}"))?;
        if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("invalid service token".into());
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            base,
            token: token.to_owned(),
        })
    }

    async fn get(
        &self,
        actor: &ForgeActor,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<Value, ForgeError> {
        // The principal name goes into a header: refuse anything that is not a plain login.
        let p = &actor.principal;
        if p.is_empty()
            || p.len() > 128
            || !p
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(ForgeError::NotFound);
        }
        let mut url = self.base.clone();
        {
            let mut s = url
                .path_segments_mut()
                .map_err(|()| ForgeError::Unavailable("bad base url".into()))?;
            s.pop_if_empty().push("api").push("v1").extend(segments);
        }
        for (k, v) in query {
            url.query_pairs_mut().append_pair(k, v);
        }
        let mut resp = self
            .http
            .get(url)
            .header("authorization", format!("token {}", self.token))
            .header("sudo", p)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| ForgeError::Unavailable(e.without_url().to_string()))?;
        let status = resp.status();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ForgeError::Unavailable(e.without_url().to_string()))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(ForgeError::Unavailable("the answer is too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        match status.as_u16() {
            200 => serde_json::from_slice(&body)
                .map_err(|_| ForgeError::Unavailable("undecodable answer".into())),
            403 | 404 => Err(ForgeError::NotFound),
            s => Err(ForgeError::Unavailable(format!("status {s}"))),
        }
    }

    async fn can_write(&self, actor: &ForgeActor, repo: &str, login: &str) -> Option<bool> {
        let (o, n) = repo.split_once('/')?;
        if login.is_empty() {
            return None;
        }
        let v = self
            .get(
                actor,
                &["repos", o, n, "collaborators", login, "permission"],
                &[],
            )
            .await
            .ok()?;
        Some(matches!(
            v.get("permission").and_then(Value::as_str),
            Some("write" | "admin" | "owner")
        ))
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn login(v: &Value) -> String {
    v.get("user").map(|u| s(u, "login")).unwrap_or_default()
}

#[async_trait]
impl ForgeAdapter for ForgejoAdapter {
    fn name(&self) -> &'static str {
        "forgejo"
    }

    async fn issues(
        &self,
        actor: &ForgeActor,
        repo: &str,
        state: &str,
        limit: usize,
    ) -> Result<Vec<ForgeItem>, ForgeError> {
        let (o, n) = repo.split_once('/').ok_or(ForgeError::NotFound)?;
        let v = self
            .get(
                actor,
                &["repos", o, n, "issues"],
                &[
                    ("type", "issues".into()),
                    ("state", state.into()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;
        let mut out = Vec::new();
        for it in v.as_array().into_iter().flatten().take(limit) {
            let who = login(it);
            out.push(ForgeItem {
                id: it.get("number").and_then(Value::as_u64).unwrap_or(0),
                title: s(it, "title"),
                state: s(it, "state"),
                body: s(it, "body"),
                author_can_write: self.can_write(actor, repo, &who).await,
                author: who,
            });
        }
        Ok(out)
    }

    async fn issue(
        &self,
        actor: &ForgeActor,
        repo: &str,
        number: u64,
    ) -> Result<ForgeItem, ForgeError> {
        let (o, n) = repo.split_once('/').ok_or(ForgeError::NotFound)?;
        let it = self
            .get(actor, &["repos", o, n, "issues", &number.to_string()], &[])
            .await?;
        let who = login(&it);
        Ok(ForgeItem {
            id: number,
            title: s(&it, "title"),
            state: s(&it, "state"),
            body: s(&it, "body"),
            author_can_write: self.can_write(actor, repo, &who).await,
            author: who,
        })
    }

    async fn pr_comments(
        &self,
        actor: &ForgeActor,
        repo: &str,
        number: u64,
        limit: usize,
    ) -> Result<(Vec<ForgeItem>, Handles), ForgeError> {
        let (o, n) = repo.split_once('/').ok_or(ForgeError::NotFound)?;
        let num = number.to_string();
        let pr = self
            .get(actor, &["repos", o, n, "pulls", &num], &[])
            .await?;
        let handles = Handles {
            head: pr
                .pointer("/head/sha")
                .and_then(Value::as_str)
                .map(str::to_owned),
            merge_base: pr
                .get("merge_base")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Handles::default()
        };
        // A PR's conversation comments are its issue comments.
        let v = self
            .get(actor, &["repos", o, n, "issues", &num, "comments"], &[])
            .await?;
        let mut out = Vec::new();
        for c in v.as_array().into_iter().flatten().take(limit) {
            let who = login(c);
            out.push(ForgeItem {
                id: c.get("id").and_then(Value::as_u64).unwrap_or(0),
                body: s(c, "body"),
                author_can_write: self.can_write(actor, repo, &who).await,
                author: who,
                ..ForgeItem::default()
            });
        }
        Ok((out, handles))
    }

    async fn actions_runs(
        &self,
        actor: &ForgeActor,
        repo: &str,
        limit: usize,
    ) -> Result<Vec<ForgeItem>, ForgeError> {
        let (o, n) = repo.split_once('/').ok_or(ForgeError::NotFound)?;
        let v = self
            .get(
                actor,
                &["repos", o, n, "actions", "runs"],
                &[("limit", limit.to_string())],
            )
            .await?;
        let mut out = Vec::new();
        for r in v
            .get("workflow_runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(limit)
        {
            // A run title comes from a commit or a workflow a user wrote, so lockdown judges the
            // actor like any other author.
            let who = r.get("actor").map(|a| s(a, "login")).unwrap_or_default();
            out.push(ForgeItem {
                id: r.get("id").and_then(Value::as_u64).unwrap_or(0),
                title: s(r, "title"),
                state: s(r, "status"),
                author_can_write: self.can_write(actor, repo, &who).await,
                author: who,
                ..ForgeItem::default()
            });
        }
        Ok(out)
    }
}
