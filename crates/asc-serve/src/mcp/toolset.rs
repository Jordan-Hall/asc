//! What `/mcp` exposes, as `[mcp]` configures it (SPEC 22.3, 22.8).
//!
//! * [`exposed`] turns `toolsets` and `tools` into the handler configuration. The default is
//!   every read toolset; the write tools are governed by `[mcp] write` and the caller's rights.
//! * [`ForgeLayer`] adds the `forge_*` tools (see [`super::forge`]) in front of the handler when
//!   `[mcp.forge] adapter` names one. The handler's catalogue knows no forge, so the layer lists
//!   the tools itself and answers their calls; every other message passes through untouched.
//!   `[mcp] tools` filters the forge tools like any other.

use std::collections::BTreeSet;
use std::sync::Arc;

use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::config::{McpConfig, parse_toolsets};
use asc_mcp::errors::{ToolError, code};
use asc_mcp::handler::{Caller, Notifier};
use asc_mcp::shape::HARD_MAX_TOKENS;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::Service;
use super::backend::RepoGate;
use super::forge::{ForgeAdapter, ForgeBackend, ForgeConfig};
use crate::config::ServeConfig;

/// The handler configuration `[mcp]` asks for. An unknown name was refused when the file was
/// loaded; here a name that no longer parses only narrows the set.
#[must_use]
pub fn exposed(cfg: &ServeConfig) -> McpConfig {
    McpConfig {
        toolsets: parse_toolsets(&cfg.mcp.toolsets.join(",")),
        tools: cfg
            .mcp
            .tools
            .as_ref()
            .map(|t| t.iter().cloned().collect::<BTreeSet<_>>()),
        max_tokens: u64::from(cfg.mcp.max_tokens).min(HARD_MAX_TOKENS),
        ..McpConfig::default()
    }
}

/// A backend that serves nothing: the forge layer only ever asks it for `forge_*` tools, which
/// [`ForgeBackend`] answers before it would reach this.
struct Nothing;

#[async_trait::async_trait]
impl McpBackend for Nothing {
    async fn call(&self, tool: &str, _: &Value, _: &CallContext) -> Result<Reply, ToolError> {
        Err(ToolError::new(
            code::NOT_FOUND,
            format!("unknown tool: {tool}"),
        ))
    }
}

/// Lists and answers the `forge_*` tools in front of another [`Service`].
struct ForgeLayer {
    inner: Arc<dyn Service>,
    forge: Arc<ForgeBackend<Nothing>>,
    tools: Vec<Value>,
    budget_bytes: u64,
}

/// `inner`, with the `forge_*` tools in front when `adapter` is set and `allowed` (the `[mcp]
/// tools` filter) leaves at least one of them. Otherwise `inner` itself.
pub(super) fn with_forge(
    inner: Arc<dyn Service>,
    gate: Arc<dyn RepoGate>,
    adapter: Option<Arc<dyn ForgeAdapter>>,
    cfg: &ForgeConfig,
    allowed: Option<&BTreeSet<String>>,
    max_tokens: u64,
) -> Arc<dyn Service> {
    let forge = ForgeBackend::new(Nothing, gate, adapter, cfg);
    let tools: Vec<Value> = forge
        .tools()
        .into_iter()
        .filter(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| allowed.is_none_or(|a| a.contains(n)))
        })
        .collect();
    if tools.is_empty() {
        return inner;
    }
    Arc::new(ForgeLayer {
        inner,
        forge: Arc::new(forge),
        tools,
        budget_bytes: max_tokens.saturating_mul(3),
    })
}

async fn call_tool(
    forge: Arc<ForgeBackend<Nothing>>,
    budget_bytes: u64,
    msg: Value,
    caller: Caller,
) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let name = msg
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = msg
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let ctx = CallContext {
        principal: caller.principal.clone(),
        budget_bytes,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    };
    let result = match forge.call(name, &args, &ctx).await {
        Ok(Reply::Messages(mut s)) => match s.next().await {
            Some(Ok(v)) => ok_result(fit(v, budget_bytes)),
            Some(Err(e)) => e.to_result(),
            None => ok_result(json!({})),
        },
        Ok(Reply::Verb { .. }) => ToolError::internal("unexpected reply").to_result(),
        Err(e) => e.to_result(),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn ok_result(v: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": v.to_string() }],
        "structuredContent": v,
        "isError": false,
    })
}

/// Drops trailing items until the result fits `budget` bytes, and says so.
fn fit(mut v: Value, budget: u64) -> Value {
    let mut cut = false;
    while v.to_string().len() as u64 > budget {
        let Some(list) = v
            .pointer_mut("/items/list")
            .and_then(Value::as_array_mut)
            .filter(|l| !l.is_empty())
        else {
            break;
        };
        list.pop();
        cut = true;
    }
    if cut {
        v["truncated"] = Value::Bool(true);
    }
    v
}

impl Service for ForgeLayer {
    fn call(&self, msg: Value, caller: Caller, out: Notifier) -> BoxFuture<'static, Option<Value>> {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let name = msg
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if method == "tools/call" && self.tools.iter().any(|t| t["name"] == name) {
            // The credential edge has authorised the repository of this very call.
            let (forge, budget) = (Arc::clone(&self.forge), self.budget_bytes);
            return Box::pin(async move { Some(call_tool(forge, budget, msg, caller).await) });
        }
        let list = method == "tools/list";
        let fut = self.inner.call(msg, caller, out);
        if !list {
            return fut;
        }
        let tools = self.tools.clone();
        Box::pin(async move {
            let mut reply = fut.await?;
            if let Some(arr) = reply
                .pointer_mut("/result/tools")
                .and_then(Value::as_array_mut)
            {
                arr.extend(tools);
            }
            Some(reply)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::super::forge::{ForgeActor, ForgeError, ForgeItem, Handles};
    use super::*;

    struct Inner(Mutex<Vec<String>>);

    impl Service for Inner {
        fn call(&self, msg: Value, _: Caller, _: Notifier) -> BoxFuture<'static, Option<Value>> {
            let method = msg["method"].as_str().unwrap_or("").to_owned();
            self.0.lock().unwrap().push(method.clone());
            Box::pin(async move {
                Some(match method.as_str() {
                    "tools/list" => json!({"id": 1, "result": {"tools": [{"name": "log"}]}}),
                    _ => json!({"id": 1, "result": {"from": "inner"}}),
                })
            })
        }
    }

    struct Gate;

    #[async_trait]
    impl RepoGate for Gate {
        async fn can_read(&self, principal: &str, repo: &str) -> bool {
            principal == "alice" && repo == "o/r"
        }
    }

    struct Fake;

    #[async_trait]
    impl ForgeAdapter for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn issues(
            &self,
            _: &ForgeActor,
            _: &str,
            _: &str,
            _: usize,
        ) -> Result<Vec<ForgeItem>, ForgeError> {
            Ok((1..=40)
                .map(|id| ForgeItem {
                    id,
                    title: "t".into(),
                    body: "x".repeat(2000),
                    author: "maint".into(),
                    author_can_write: Some(true),
                    ..ForgeItem::default()
                })
                .collect())
        }
        async fn issue(&self, _: &ForgeActor, _: &str, _: u64) -> Result<ForgeItem, ForgeError> {
            Err(ForgeError::NotFound)
        }
        async fn pr_comments(
            &self,
            _: &ForgeActor,
            _: &str,
            _: u64,
            _: usize,
        ) -> Result<(Vec<ForgeItem>, Handles), ForgeError> {
            Err(ForgeError::NotFound)
        }
        async fn actions_runs(
            &self,
            _: &ForgeActor,
            _: &str,
            _: usize,
        ) -> Result<Vec<ForgeItem>, ForgeError> {
            Ok(vec![])
        }
    }

    fn layer(allowed: Option<BTreeSet<String>>, max_tokens: u64) -> (Arc<dyn Service>, Arc<Inner>) {
        let inner = Arc::new(Inner(Mutex::new(Vec::new())));
        let svc = with_forge(
            inner.clone(),
            Arc::new(Gate),
            Some(Arc::new(Fake)),
            &ForgeConfig::default(),
            allowed.as_ref(),
            max_tokens,
        );
        (svc, inner)
    }

    async fn send(svc: &Arc<dyn Service>, msg: Value, who: &str) -> Value {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let caller = Caller {
            principal: Some(who.into()),
            server_write: false,
        };
        svc.call(msg, caller, tx).await.unwrap()
    }

    fn call(tool: &str, repo: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
               "params": {"name": tool, "arguments": {"repo": repo, "limit": 40}}})
    }

    fn names(v: &Value) -> Vec<&str> {
        v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn the_forge_tools_join_the_list_and_nothing_else_changes() {
        let (svc, inner) = layer(None, 6000);
        let v = send(&svc, json!({"id": 1, "method": "tools/list"}), "alice").await;
        assert_eq!(
            names(&v),
            [
                "log",
                "forge_issues",
                "forge_issue",
                "forge_pr_comments",
                "forge_actions_runs"
            ]
        );
        let v = send(&svc, json!({"id": 2, "method": "ping"}), "alice").await;
        assert_eq!(v["result"]["from"], "inner");
        assert_eq!(inner.0.lock().unwrap().as_slice(), ["tools/list", "ping"]);
    }

    #[tokio::test]
    async fn a_forge_call_is_answered_by_the_layer_for_a_readable_repository_only() {
        let (svc, inner) = layer(None, 6000);
        let v = send(&svc, call("forge_issues", "o/r"), "alice").await;
        assert_eq!(v["id"], 9);
        assert_eq!(v["result"]["isError"], false, "{v}");
        let list = v["result"]["structuredContent"]["items"]["list"]
            .as_array()
            .unwrap();
        // 40 items of 2 KB do not fit 6000 tokens of budget: the tail is dropped and said so.
        assert!(!list.is_empty() && list.len() < 40, "{}", list.len());
        assert_eq!(v["result"]["structuredContent"]["truncated"], true);
        // Someone who cannot read the repository gets the answer a missing one gets.
        let v = send(&svc, call("forge_issues", "o/r"), "mallory").await;
        assert_eq!(v["result"]["isError"], true);
        assert_eq!(
            v["result"]["structuredContent"]["error"]["code"],
            "NOT_FOUND"
        );
        assert!(
            inner.0.lock().unwrap().is_empty(),
            "the handler never saw a forge call"
        );
    }

    #[tokio::test]
    async fn the_tools_filter_hides_a_forge_tool_from_list_and_call() {
        let only = BTreeSet::from(["forge_issue".to_owned(), "log".to_owned()]);
        let (svc, inner) = layer(Some(only), 6000);
        let v = send(&svc, json!({"id": 1, "method": "tools/list"}), "alice").await;
        assert_eq!(names(&v), ["log", "forge_issue"]);
        // A tool the filter removed falls through to the handler, which refuses it.
        let _ = send(&svc, call("forge_issues", "o/r"), "alice").await;
        assert_eq!(inner.0.lock().unwrap().last().unwrap(), "tools/call");
    }

    #[test]
    fn nothing_is_wrapped_without_an_adapter_or_a_forge_tool() {
        let inner: Arc<dyn Service> = Arc::new(Inner(Mutex::new(Vec::new())));
        let only = BTreeSet::from(["log".to_owned()]);
        let cfg = ForgeConfig::default();
        let filtered = with_forge(
            inner.clone(),
            Arc::new(Gate),
            Some(Arc::new(Fake)),
            &cfg,
            Some(&only),
            6000,
        );
        assert!(Arc::ptr_eq(&inner, &filtered));
        let none = with_forge(inner.clone(), Arc::new(Gate), None, &cfg, None, 6000);
        assert!(Arc::ptr_eq(&inner, &none), "no adapter, no forge tools");
    }

    #[test]
    fn the_configuration_picks_the_toolsets_and_tools() {
        let mut cfg = ServeConfig::default();
        let all = exposed(&cfg);
        assert_eq!(all.toolsets.len(), 5);
        assert!(all.tools.is_none());
        cfg.mcp.toolsets = vec!["core".into()];
        cfg.mcp.tools = Some(vec!["log".into()]);
        cfg.mcp.max_tokens = 99_999_999;
        let one = exposed(&cfg);
        assert_eq!(one.toolsets.len(), 1);
        assert!(one.tool_allowed("log") && !one.tool_allowed("diff"));
        assert_eq!(one.max_tokens, HARD_MAX_TOKENS);
    }
}
