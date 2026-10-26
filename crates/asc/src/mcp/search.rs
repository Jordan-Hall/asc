//! `search` for `asc mcp` (SPEC 22.3.2 and 23.4.4, unit I21): code search on the asc server that the
//! workspace's `origin` names.
//!
//! The tool is the one in `asc_mcp::search`; this module is its [`CodeIntelClient`]. The server is
//! reached the way `asc search` reaches it ([`connect`], the credential resolver of SPEC 15.6.1),
//! with one difference: an `asc mcp` process owns standard input, so a credential is never asked for
//! on a terminal ([`Prompt::Never`]). The connection opens on first use; a call that is refused for
//! its credential asks the helpers once more (the same one re-ask the other commands make).
//!
//! The tool is listed only if the server advertises `search.v1`. [`ServerSearch::advertises`] asks
//! once at start, with a bound on the wait, and only when the `search` toolset is enabled. A server
//! that cannot be reached then simply lists no `search` tool.
//!
//! The scope of a call is the server's: a revision, a change or a stack, by default the repository's
//! default branch. Uncommitted work is not searched (`grep` and `read_file` read the working copy).

use std::sync::Arc;
use std::time::Duration;

use asc_git::realgit::RealGit;
use asc_mcp::backend::CallContext;
use asc_mcp::errors::{ToolError, code};
use asc_mcp::rpc_backend::stream_call;
use asc_mcp::search::{self, CodeIntelClient, Plan};
use asc_rpc::IdMode;
use asc_state::StateDb;
use futures::stream::BoxStream;
use gix_config::File as Config;
use prost_reflect::DescriptorPool;
use serde_json::Value;
use tokio::sync::OnceCell;

use super::pathgate::Gate;
use crate::cli::common::{Ctx, WsCtx};
use crate::credential::{self, Prompt};
use crate::remote::server::{ServerRemote, cfg_get, connect};

/// Longest one call may run.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest the start-up question about `search.v1` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Longest error text kept.
const MAX_ERROR_TEXT: usize = 512;
/// The remote that serves the search.
const REMOTE: &str = "origin";
/// The result lists of the three `CodeIntel` responses; each item has a `path`.
const RESULT_LISTS: [&str; 3] = ["files", "symbols", "chunks"];

struct State {
    config: Config,
    db: StateDb,
    real: Option<RealGit>,
    pool: DescriptorPool,
    remote: OnceCell<Arc<ServerRemote>>,
}

/// The `CodeIntel` client of `asc mcp`.
#[derive(Clone)]
pub struct ServerSearch(Arc<State>);

fn unavailable(text: &str) -> ToolError {
    ToolError::not_available(
        text.chars()
            .filter(|c| !c.is_control())
            .take(MAX_ERROR_TEXT)
            .collect::<String>(),
    )
}

impl ServerSearch {
    /// The client for `ws`, or `None` when the repository has no `origin` to ask.
    #[must_use]
    pub fn new(ctx: &Ctx, ws: &WsCtx) -> Option<Self> {
        cfg_get(ctx.config.file(), &format!("remote.{REMOTE}.url"))?;
        let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).ok()?;
        Some(Self(Arc::new(State {
            config: ctx.config.file().clone(),
            db: ws.db.clone(),
            real: ctx.real_git().ok().cloned(),
            pool,
            remote: OnceCell::new(),
        })))
    }

    async fn remote(&self) -> Result<&Arc<ServerRemote>, ToolError> {
        let s = &*self.0;
        s.remote
            .get_or_try_init(|| async {
                let stored = cfg_get(&s.config, &format!("remote.{REMOTE}.url"))
                    .ok_or_else(|| unavailable("origin is not configured"))?;
                let connected = connect(
                    &s.config,
                    &s.db,
                    s.real.clone(),
                    Some(REMOTE),
                    &stored,
                    Prompt::Never,
                )
                .await
                .map_err(|e| unavailable(&e.to_string()))?
                .ok_or_else(|| unavailable("origin is not an asc server"))?;
                Ok(Arc::new(ServerRemote::new(REMOTE, connected)))
            })
            .await
    }

    /// Whether the server advertises `search.v1`. False when it cannot be reached in time.
    pub async fn advertises(&self) -> bool {
        let probe = async {
            let version = self.remote().await.ok()?.client().server_version().await;
            Some(version.ok()?.features.iter().any(|f| f == search::FEATURE))
        };
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, probe).await,
            Ok(Some(true))
        )
    }
}

#[async_trait::async_trait]
impl CodeIntelClient for ServerSearch {
    async fn call(
        &self,
        plan: Plan,
        ctx: &CallContext,
    ) -> Result<BoxStream<'static, Result<Value, ToolError>>, ToolError> {
        let remote = self.remote().await?;
        let method = self
            .0
            .pool
            .get_service_by_name("asc.v1.CodeIntel")
            .and_then(|s| s.methods().find(|m| m.name() == plan.rpc.method()))
            .ok_or_else(|| ToolError::internal("CodeIntel is missing from the descriptor set"))?;
        let mut asked_again = false;
        loop {
            let session = remote.session();
            if plan
                .repo
                .as_deref()
                .is_some_and(|want| want != session.wire.repo)
            {
                return Err(ToolError::new(
                    code::PATH_POLICY,
                    "the repository is fixed by the connection",
                ));
            }
            let msg = search::wire_request(&plan, &method, &session.wire.repo, IdMode::Asc)?;
            let mut req = session.wire.request(msg, Some(uuid::Uuid::now_v7()));
            req.set_timeout(CALL_TIMEOUT);
            match stream_call(&method, session.wire.channel.clone(), req, ctx).await {
                Ok(stream) => {
                    credential::report_result(&session.resolved, true);
                    return Ok(stream);
                }
                Err(e) if e.code == "UNAUTHENTICATED" && !asked_again => {
                    asked_again = true;
                    remote
                        .reauth(&session)
                        .map_err(|e| ToolError::new("UNAUTHENTICATED", e.to_string()))?;
                }
                Err(e) => {
                    if e.code == "UNAUTHENTICATED" {
                        credential::report_result(&session.resolved, false);
                    }
                    return Err(e);
                }
            }
        }
    }
}

/// Removes the results whose path the work tree's rules hide (ignored files and `.git`), the same
/// rule `grep` applies: a path found in a revision is shown only if the work tree would show it.
pub fn hide_ignored(gate: &Gate, mut message: Value) -> Value {
    for list in RESULT_LISTS {
        if let Some(items) = message.get_mut(list).and_then(Value::as_array_mut) {
            items.retain(|item| {
                item.get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|p| gate.shown_ok(p))
            });
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use asc_wc::ignore::IgnoreSources;
    use serde_json::json;

    use super::*;

    fn gate(dir: &std::path::Path) -> Gate {
        Gate::new(
            dir.to_owned(),
            IgnoreSources {
                asc_excludes: dir.join("no-such-asc-ignore"),
                core_excludes: None,
                local_exclude: dir.join(".asc/info/exclude"),
                common_exclude: dir.join("no-such-exclude"),
            },
        )
    }

    #[test]
    fn ignored_and_unnamed_results_are_dropped_in_every_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "secret/\n*.log\n").unwrap();
        let g = gate(dir.path());
        let m = json!({
            "scope": { "coverage": 1.0 },
            "files": [{ "path": "src/a.rs" }, { "path": "secret/k.txt" }, { "path": ".git/config" }, { "path": { "b64": "/w==" } }],
            "symbols": [{ "path": "build.log" }, { "path": "lib/b.rs" }],
            "chunks": [{ "path": "src/a.rs" }, { "path": "secret/deep/x.rs" }],
        });
        let out = hide_ignored(&g, m);
        assert_eq!(out["files"], json!([{ "path": "src/a.rs" }]));
        assert_eq!(out["symbols"], json!([{ "path": "lib/b.rs" }]));
        assert_eq!(out["chunks"], json!([{ "path": "src/a.rs" }]));
        // Everything else is left alone.
        assert_eq!(out["scope"]["coverage"], 1.0);
    }
}
