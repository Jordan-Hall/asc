//! The seam between the protocol layer and the repository (SPEC 22.7.2).
//!
//! `asc-mcp` holds no repository logic. A backend answers a tool call with a stream of messages in
//! the asc JSON profile (`json.profile.v1`), or with the exit code and `--json` output of a child
//! verb. `LocalBackend` (in `asc`), `ServeBackend` (in `asc-serve`) and `RpcBackend` (M08) implement
//! [`McpBackend`].

use std::path::PathBuf;

use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cursor::Position;
use crate::errors::ToolError;

/// One progress report; the handler turns it into `notifications/progress`.
#[derive(Debug, Clone)]
pub struct Progress {
    tx: mpsc::Sender<Value>,
    token: Value,
}

impl Progress {
    pub(crate) fn new(tx: mpsc::Sender<Value>, token: Value) -> Self {
        Self { tx, token }
    }

    /// Reports `progress` (and `total` when known). Reports are dropped, not queued, when the
    /// client reads slowly: progress is advisory.
    pub fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let mut params = serde_json::json!({ "progressToken": self.token, "progress": progress });
        if let Some(t) = total {
            params["total"] = t.into();
        }
        if let Some(m) = message {
            params["message"] = crate::errors::redact(m).into();
        }
        let _ = self.tx.try_send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        }));
    }
}

/// What the backend needs to know about one call besides the arguments.
#[derive(Debug, Clone)]
pub struct CallContext {
    /// The authenticated principal (HTTP); `None` on stdio, where the workspace is the user's.
    pub principal: Option<String>,
    /// The tool's byte budget for this call, `max_tokens * 3` (SPEC 22.6.2). A backend stops
    /// reading a stream once this is spent; the handler folds under the same budget.
    pub budget_bytes: u64,
    /// The position decoded from a verified cursor, if the call carried one.
    pub position: Option<Position>,
    /// Cancelled when the client sends `notifications/cancelled` or closes the stream. A backend
    /// stops its RPC stream or child verb (SIGTERM, 3 s, kill) when this fires.
    pub cancel: CancellationToken,
    /// Present when the client set a `progressToken`.
    pub progress: Option<Progress>,
}

/// What a backend returns for a call.
pub enum Reply {
    /// Messages of a (streamed) RPC, each a JSON object in the asc JSON profile. The handler merges
    /// them (repeated fields concatenated) and drops the stream when the budget is spent.
    Messages(BoxStream<'static, Result<Value, ToolError>>),
    /// A child verb's exit code and its `--json` output (SPEC 22.3.1: exit 1 and 2 are results).
    Verb {
        /// Process exit code.
        exit_code: i32,
        /// The captured `--json` result.
        result: Value,
    },
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Messages(_) => f.write_str("Reply::Messages(..)"),
            Self::Verb { exit_code, .. } => write!(f, "Reply::Verb({exit_code})"),
        }
    }
}

/// The repository side of the MCP server.
#[async_trait::async_trait]
pub trait McpBackend: Send + Sync + 'static {
    /// Runs tool `tool` with validated `args` (the arguments object, `cursor`, `max_tokens` and
    /// `worktree` still in place). The backend maps the tool to its RPC or child verb through the
    /// catalogue row and returns messages already in the JSON profile.
    ///
    /// # Errors
    /// A [`ToolError`], which the handler returns as an `isError` result.
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError>;

    /// The canonical worktree paths of the launch workspace (`Workspace.Worktrees`), the only
    /// values the `worktree` argument may take. A server backend returns an empty list.
    async fn worktrees(&self) -> Result<Vec<PathBuf>, ToolError> {
        Ok(Vec::new())
    }
}
