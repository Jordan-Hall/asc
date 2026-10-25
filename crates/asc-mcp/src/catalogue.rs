//! The tool catalogue and its deterministic `tools/list` (SPEC 22.3, 22.4.1).
//!
//! The table comes from `tools.toml` through `build.rs`. [`Context`] says what one server
//! instance may show: the surface, the enabled toolsets, the write level, whether the workspace
//! uploads, the server's write rights and the features the server advertises. [`tools`] filters
//! the table by it and [`tools_list`] renders what `tools/list` returns. The same context always
//! gives the same list in the same order.

use std::collections::BTreeSet;

use rmcp::model::Tool;
use serde_json::{Value, json};

use crate::schema::Surface;

/// A group of tools enabled together (SPEC 22.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Toolset {
    /// Eleven read tools; the default set.
    Core,
    /// `line_history`, `interdiff`, `compare`.
    History,
    /// Stack and operation tools.
    Stack,
    /// `project_graph`.
    Projects,
    /// `search`, listed when `search.v1` is advertised (M15).
    Search,
    /// Tools that change state; listed only when writing is enabled.
    Write,
}

impl Toolset {
    /// The name used in configuration (`asc.mcp.toolsets`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::History => "history",
            Self::Stack => "stack",
            Self::Projects => "projects",
            Self::Search => "search",
            Self::Write => "write",
        }
    }

    /// Parses a configuration name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }

    /// Every toolset, in listing order.
    pub const ALL: [Self; 6] = [
        Self::Core,
        Self::History,
        Self::Stack,
        Self::Projects,
        Self::Search,
        Self::Write,
    ];
}

/// Tool annotation hints (MCP `ToolAnnotations`); all four are always sent explicitly because
/// the protocol defaults `destructiveHint` to true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ann {
    /// The tool does not modify its environment.
    pub read_only: bool,
    /// Repeating the call has no further effect.
    pub idempotent: bool,
    /// The tool may destroy data.
    pub destructive: bool,
    /// The tool reaches a server or other open world.
    pub open_world: bool,
}

/// When a call needs the user's confirmation (SPEC 22.5, enforced by M06).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Never.
    Never,
    /// Always.
    Always,
    /// As `asc.mcp.confirm` says.
    Config,
    /// When the named boolean argument is true.
    Field(&'static str),
    /// When `update_ref` is `refs/for/*`.
    RefsFor,
    /// When `update_ref` is set and the call is not a dry run.
    RefsApply,
}

/// What a write tool needs before it is listed (SPEC 22.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// A read tool.
    None,
    /// `write=local` or higher.
    WriteLocal,
    /// `write=remote`.
    WriteRemote,
    /// `write=local` while the workspace does not upload, else `write=remote`.
    WriteLocalOrRemote,
    /// The `asc:write` scope on the server.
    ServerWrite,
    /// The `asc:write` scope and `[mcp] write` on the server.
    ServerWriteMcp,
}

/// One row of the generated table.
#[derive(Debug, Clone, Copy)]
pub struct ToolDef {
    /// Tool name, `[a-z_]+`.
    pub name: &'static str,
    /// Its toolset.
    pub toolset: Toolset,
    /// Bit 1: stdio, bit 2: http.
    pub surfaces: u8,
    /// Backing call, for documentation.
    pub rpc: &'static str,
    /// Full request message name; empty when the input is only added fields.
    pub request: &'static str,
    /// Full response message name.
    pub response: &'static str,
    /// Description shown to the model.
    pub description: &'static str,
    /// Annotation hints.
    pub ann: Ann,
    /// Confirmation rule.
    pub confirm: Confirm,
    /// Listing requirement.
    pub needs: Needs,
    /// A `Version.features` entry the tool needs.
    pub requires_feature: Option<&'static str>,
    /// `inputSchema` on stdio (empty when the tool is not on stdio).
    pub input_stdio: &'static str,
    /// `inputSchema` on http (empty when the tool is not on http).
    pub input_http: &'static str,
    /// `outputSchema`.
    pub output: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/tools_gen.rs"));

/// The surface bit in [`ToolDef::surfaces`].
const fn bit(surface: Surface) -> u8 {
    match surface {
        Surface::Stdio => 1,
        Surface::Http => 2,
    }
}

impl ToolDef {
    /// Whether the tool exists on `surface`.
    #[must_use]
    pub const fn on(&self, surface: Surface) -> bool {
        self.surfaces & bit(surface) != 0
    }

    /// The `inputSchema` JSON text for `surface`, or `None` when the tool is not there.
    #[must_use]
    pub const fn input_schema(&self, surface: Surface) -> Option<&'static str> {
        if !self.on(surface) {
            return None;
        }
        Some(match surface {
            Surface::Stdio => self.input_stdio,
            Surface::Http => self.input_http,
        })
    }
}

/// How far the user allows writing (`asc.mcp.write`, SPEC 22.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum WriteLevel {
    /// No write tool is listed.
    #[default]
    None,
    /// Verbs that never touch the network.
    Local,
    /// Verbs that upload as well.
    Remote,
}

/// What one server instance shows. The default is the stdio server with the `core` toolset and no
/// writing.
#[derive(Debug, Clone)]
pub struct Context {
    /// The surface.
    pub surface: Surface,
    /// Enabled read toolsets. `Write` is controlled by [`Context::write`] instead.
    pub toolsets: BTreeSet<Toolset>,
    /// The write level (stdio).
    pub write: WriteLevel,
    /// Whether this workspace uploads resulting drafts at once (online branchless mode).
    pub workspace_uploads: bool,
    /// The caller has the `asc:write` scope (http).
    pub server_write: bool,
    /// The deployment sets `[mcp] write` (http).
    pub server_mcp_write: bool,
    /// `--read-only`: no write tool, whatever else is set.
    pub read_only: bool,
    /// `Version.features` of the server in use.
    pub features: BTreeSet<String>,
}

impl Context {
    /// The default for a surface: only `core`, nothing writable, no features.
    #[must_use]
    pub fn new(surface: Surface) -> Self {
        Self {
            surface,
            toolsets: BTreeSet::from([Toolset::Core]),
            write: WriteLevel::None,
            workspace_uploads: false,
            server_write: false,
            server_mcp_write: false,
            read_only: false,
            features: BTreeSet::new(),
        }
    }

    /// Enables every read toolset and `search`'s feature-gated set.
    #[must_use]
    pub fn with_all_reads(mut self) -> Self {
        self.toolsets = [Toolset::Core, Toolset::History, Toolset::Stack, Toolset::Projects, Toolset::Search]
            .into_iter()
            .collect();
        self
    }

    fn write_allows(&self, needs: Needs) -> bool {
        let level = |l: WriteLevel| self.write >= l;
        match needs {
            Needs::None => true,
            Needs::WriteLocal => level(WriteLevel::Local),
            Needs::WriteRemote => level(WriteLevel::Remote),
            Needs::WriteLocalOrRemote if self.workspace_uploads => level(WriteLevel::Remote),
            Needs::WriteLocalOrRemote => level(WriteLevel::Local),
            Needs::ServerWrite => self.server_write,
            Needs::ServerWriteMcp => self.server_write && self.server_mcp_write,
        }
    }

    /// Whether `def` is listed under this context.
    #[must_use]
    pub fn lists(&self, def: &ToolDef) -> bool {
        if !def.on(self.surface) {
            return false;
        }
        if def.toolset == Toolset::Write {
            if self.read_only || !self.write_allows(def.needs) {
                return false;
            }
            // On the server the write rights are the scopes; a stdio server needs a write level.
            if self.surface == Surface::Stdio && self.write == WriteLevel::None {
                return false;
            }
        } else if !self.toolsets.contains(&def.toolset) {
            return false;
        }
        def.requires_feature.is_none_or(|f| self.features.contains(f))
    }
}

/// A tool as listed: the table row and the annotations computed for this workspace.
#[derive(Debug, Clone, Copy)]
pub struct Listed {
    /// The table row.
    pub def: &'static ToolDef,
    /// Annotations with the per-workspace `openWorldHint` applied (SPEC 22.3.3).
    pub ann: Ann,
}

/// The tools listed under `ctx`, in toolset order and then table order.
#[must_use]
pub fn tools(ctx: &Context) -> Vec<Listed> {
    let mut out: Vec<Listed> = TOOLS
        .iter()
        .filter(|d| ctx.lists(d))
        .map(|def| {
            let mut ann = def.ann;
            // Verbs that upload at once when the workspace is online reach the world.
            if def.needs == Needs::WriteLocalOrRemote && ctx.workspace_uploads {
                ann.open_world = true;
            }
            Listed { def, ann }
        })
        .collect();
    out.sort_by_key(|l| l.def.toolset);
    out
}

/// The row of tool `name` on `surface`.
#[must_use]
pub fn find(name: &str, surface: Surface) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|d| d.name == name && d.on(surface))
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| json!({"type": "object"}))
}

/// One tool as the JSON object `tools/list` carries.
#[must_use]
pub fn tool_json(listed: &Listed, surface: Surface) -> Value {
    let def = listed.def;
    let input = def.input_schema(surface).unwrap_or("{}");
    json!({
        "name": def.name,
        "description": def.description,
        "inputSchema": parse(input),
        "outputSchema": parse(def.output),
        "annotations": {
            "readOnlyHint": listed.ann.read_only,
            "destructiveHint": listed.ann.destructive,
            "idempotentHint": listed.ann.idempotent,
            "openWorldHint": listed.ann.open_world,
        },
    })
}

/// The `tools/list` result body, `{"tools": [...]}`.
#[must_use]
pub fn tools_list_json(ctx: &Context) -> Value {
    let list: Vec<Value> = tools(ctx).iter().map(|l| tool_json(l, ctx.surface)).collect();
    json!({ "tools": list })
}

/// The `tools/list` result as rmcp tool values, in listing order.
#[must_use]
pub fn tools_list(ctx: &Context) -> Vec<Tool> {
    tools(ctx)
        .iter()
        .filter_map(|l| serde_json::from_value(tool_json(l, ctx.surface)).ok())
        .collect()
}

/// The token estimate used for budgets: bytes / 3, rounded up (SPEC 22.6.2).
#[must_use]
pub const fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(3)
}

/// The estimated tokens of the `tools/list` result under `ctx`.
#[must_use]
pub fn estimated_tokens(ctx: &Context) -> usize {
    estimate_tokens(tools_list_json(ctx).to_string().len())
}

/// The largest estimated size of the default `tools/list` (SPEC 22.4.1).
pub const DEFAULT_LIST_TOKEN_BUDGET: usize = 6_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ctx: &Context) -> Vec<&'static str> {
        tools(ctx).iter().map(|l| l.def.name).collect()
    }

    fn snapshot(name: &str, ctx: &Context) {
        insta::with_settings!({snapshot_path => "../tests/snapshots", prepend_module_to_snapshot => false}, {
            insta::assert_json_snapshot!(name, tools_list_json(ctx));
        });
    }

    fn write_ctx(surface: Surface, level: WriteLevel) -> Context {
        let mut c = Context::new(surface);
        c.toolsets = BTreeSet::new();
        c.write = level;
        c.server_write = true;
        c.server_mcp_write = true;
        c
    }

    #[test]
    fn default_set_is_the_eleven_core_read_tools() {
        let ctx = Context::new(Surface::Stdio);
        assert_eq!(
            names(&ctx),
            [
                "status", "diff", "log", "show", "read_file", "ls_tree", "grep", "blame",
                "smartlog", "change_info", "affected"
            ]
        );
        assert!(tools(&ctx).iter().all(|l| l.ann.read_only && l.def.toolset == Toolset::Core));
        // The server has the same eleven (SPEC 22.3.2 lists `smartlog` for L only).
        let http = names(&Context::new(Surface::Http));
        assert!(http.contains(&"diff") && http.contains(&"blame") && http.contains(&"affected"));
        assert!(!http.contains(&"smartlog") && !http.contains(&"status"));
    }

    #[test]
    fn default_list_fits_the_token_budget() {
        for surface in [Surface::Stdio, Surface::Http] {
            let ctx = Context::new(surface);
            let est = estimated_tokens(&ctx);
            assert!(
                est <= DEFAULT_LIST_TOKEN_BUDGET,
                "{surface:?} default tools/list estimates {est} tokens"
            );
        }
    }

    #[test]
    fn names_are_valid_and_unique_per_surface() {
        let mut seen = BTreeSet::new();
        for d in TOOLS {
            assert!(!d.name.is_empty() && d.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{}", d.name);
            for s in [Surface::Stdio, Surface::Http] {
                if d.on(s) {
                    assert!(seen.insert((d.name, s as u8)), "duplicate {} on {s:?}", d.name);
                    assert!(d.input_schema(s).is_some_and(|t| t.starts_with('{')), "{} input", d.name);
                }
            }
            assert!(d.output.starts_with('{'), "{} output", d.name);
        }
    }

    #[test]
    fn schemas_are_valid_json_and_self_contained() {
        for d in TOOLS {
            for s in [Surface::Stdio, Surface::Http] {
                let Some(text) = d.input_schema(s) else { continue };
                for schema in [text, d.output] {
                    let v: Value = serde_json::from_str(schema).expect("schema is JSON");
                    assert_eq!(v["type"], "object", "{}", d.name);
                    let defs = v.get("$defs").and_then(Value::as_object);
                    for part in schema.split("\"$ref\":\"#/$defs/").skip(1) {
                        let key = part.split('"').next().unwrap();
                        assert!(defs.is_some_and(|m| m.contains_key(key)), "{}: dangling $ref {key}", d.name);
                    }
                }
            }
        }
    }

    #[test]
    fn input_projection_follows_the_spec() {
        let status = find("status", Surface::Stdio).unwrap();
        let v: Value = serde_json::from_str(status.input_stdio).unwrap();
        let p = &v["properties"];
        assert!(p.get("ws_id").is_none() && p.get("ignored").is_none());
        assert!(p.get("worktree").is_some() && p.get("max_tokens").is_some() && p.get("cursor").is_some());
        assert_eq!(p["max_files"]["default"], 500);
        assert_eq!(p["max_files"]["maximum"], 5000);

        let diff = find("diff", Surface::Http).unwrap();
        let v: Value = serde_json::from_str(diff.input_http).unwrap();
        let p = &v["properties"];
        assert!(p.get("null_terminated").is_none() && p.get("binary").is_none());
        assert_eq!(p["repo"]["x-mcp-header"], "Repo");
        assert!(p.get("worktree").is_none(), "worktree is stdio only");
        assert_eq!(p["max_files"]["maximum"], 300);
        assert_eq!(p["max_tokens"]["default"], 6000);
    }

    #[test]
    fn toolsets_are_opt_in() {
        let mut ctx = Context::new(Surface::Stdio);
        assert!(!names(&ctx).contains(&"line_history"));
        ctx.toolsets.insert(Toolset::History);
        let n = names(&ctx);
        assert_eq!(&n[n.len() - 3..], ["line_history", "interdiff", "compare"]);
        // Core stays ahead of the others, whatever the table order is.
        assert_eq!(n[0], "status");
    }

    #[test]
    fn feature_gated_tools_wait_for_the_feature() {
        // No row needs a feature yet (`search` is M15); the gate itself is exercised through a
        // synthetic row.
        let mut def = *find("status", Surface::Stdio).unwrap();
        def.requires_feature = Some("search.v1");
        let mut ctx = Context::new(Surface::Stdio);
        assert!(!ctx.lists(&def));
        ctx.features.insert("search.v1".into());
        assert!(ctx.lists(&def));
    }

    #[test]
    fn write_tools_need_the_write_level() {
        let none = write_ctx(Surface::Stdio, WriteLevel::None);
        assert!(names(&none).is_empty());
        let local = names(&write_ctx(Surface::Stdio, WriteLevel::Local));
        assert_eq!(
            local,
            [
                "stage", "create", "modify", "absorb", "restack", "undo", "redo", "hide", "unhide",
                "worktree_create"
            ]
        );
        let remote = names(&write_ctx(Surface::Stdio, WriteLevel::Remote));
        assert!(remote.contains(&"sync") && remote.contains(&"submit"));
    }

    #[test]
    fn uploading_workspaces_need_remote_for_the_change_verbs() {
        let mut ctx = write_ctx(Surface::Stdio, WriteLevel::Local);
        ctx.workspace_uploads = true;
        // Online in branchless mode only `stage` and `worktree_create` stay local.
        assert_eq!(names(&ctx), ["stage", "worktree_create"]);
        ctx.write = WriteLevel::Remote;
        let listed = tools(&ctx);
        let create = listed.iter().find(|l| l.def.name == "create").unwrap();
        assert!(create.ann.open_world, "uploading verbs reach the world");
        let stage = listed.iter().find(|l| l.def.name == "stage").unwrap();
        assert!(!stage.ann.open_world);
        ctx.workspace_uploads = false;
        let local_create = tools(&ctx).into_iter().find(|l| l.def.name == "create").unwrap();
        assert!(!local_create.ann.open_world);
    }

    #[test]
    fn read_only_wins_over_every_write_setting() {
        let mut ctx = write_ctx(Surface::Stdio, WriteLevel::Remote);
        ctx.read_only = true;
        assert!(names(&ctx).is_empty());
        let mut http = write_ctx(Surface::Http, WriteLevel::None);
        http.read_only = true;
        assert!(names(&http).is_empty());
    }

    #[test]
    fn server_write_tools_follow_the_scopes() {
        let mut ctx = Context::new(Surface::Http);
        ctx.toolsets.clear();
        assert!(names(&ctx).is_empty(), "no scope, no write tool");
        ctx.server_write = true;
        assert_eq!(names(&ctx), ["submit", "rebase", "restack"]);
        ctx.server_mcp_write = true;
        assert_eq!(names(&ctx), ["commit", "submit", "rebase", "restack"]);
    }

    #[test]
    fn annotations_match_the_spec_table() {
        let ann = |name: &str, s| find(name, s).unwrap().ann;
        assert!(ann("undo", Surface::Stdio).destructive);
        assert!(!ann("redo", Surface::Stdio).destructive);
        assert!(ann("submit", Surface::Stdio).destructive && ann("submit", Surface::Stdio).open_world);
        assert!(ann("sync", Surface::Stdio).open_world && !ann("sync", Surface::Stdio).destructive);
        assert!(ann("stage", Surface::Stdio).idempotent && !ann("stage", Surface::Stdio).destructive);
        assert!(!ann("create", Surface::Stdio).destructive && !ann("create", Surface::Stdio).idempotent);
        let list = tools_list_json(&Context::new(Surface::Stdio));
        for t in list["tools"].as_array().unwrap() {
            assert_eq!(t["annotations"]["readOnlyHint"], true);
            assert_eq!(t["annotations"]["destructiveHint"], false);
        }
    }

    #[test]
    fn confirm_rules_match_the_spec_table() {
        assert_eq!(find("submit", Surface::Stdio).unwrap().confirm, Confirm::Always);
        assert_eq!(find("submit", Surface::Http).unwrap().confirm, Confirm::Always);
        assert_eq!(find("undo", Surface::Stdio).unwrap().confirm, Confirm::Config);
        assert_eq!(find("sync", Surface::Stdio).unwrap().confirm, Confirm::Field("delete_merged"));
        assert_eq!(find("commit", Surface::Http).unwrap().confirm, Confirm::RefsFor);
        assert_eq!(find("rebase", Surface::Http).unwrap().confirm, Confirm::RefsApply);
        assert_eq!(find("create", Surface::Stdio).unwrap().confirm, Confirm::Never);
    }

    #[test]
    fn server_dry_run_defaults_to_true() {
        for name in ["rebase", "restack"] {
            let d = find(name, Surface::Http).unwrap();
            let v: Value = serde_json::from_str(d.input_http).unwrap();
            assert_eq!(v["properties"]["dry_run"]["default"], true, "{name}");
        }
    }

    #[test]
    fn listing_is_deterministic_and_converts_to_rmcp() {
        let ctx = Context::new(Surface::Stdio);
        assert_eq!(tools_list_json(&ctx).to_string(), tools_list_json(&ctx).to_string());
        let rmcp_tools = tools_list(&ctx);
        assert_eq!(rmcp_tools.len(), 11);
        assert!(rmcp_tools.iter().all(|t| t.output_schema.is_some() && t.annotations.is_some()));
    }

    #[test]
    fn hostile_text_cannot_reach_a_description() {
        // Descriptions are static strings from tools.toml; none is built from repository data.
        for d in TOOLS {
            assert!(!d.description.is_empty() && d.description.len() < 400, "{}", d.name);
            assert!(!d.description.contains('\n'), "{}", d.name);
        }
    }

    #[test]
    fn snapshot_core_stdio() {
        snapshot("tools_list_core_stdio", &Context::new(Surface::Stdio));
    }

    #[test]
    fn snapshot_core_http() {
        snapshot("tools_list_core_http", &Context::new(Surface::Http));
    }

    #[test]
    fn snapshot_history() {
        let mut ctx = Context::new(Surface::Stdio);
        ctx.toolsets = BTreeSet::from([Toolset::History]);
        snapshot("tools_list_history_stdio", &ctx);
    }

    #[test]
    fn snapshot_stack() {
        let mut ctx = Context::new(Surface::Stdio);
        ctx.toolsets = BTreeSet::from([Toolset::Stack]);
        snapshot("tools_list_stack_stdio", &ctx);
    }

    #[test]
    fn snapshot_projects() {
        let mut ctx = Context::new(Surface::Stdio);
        ctx.toolsets = BTreeSet::from([Toolset::Projects]);
        snapshot("tools_list_projects_stdio", &ctx);
    }

    #[test]
    fn snapshot_write_stdio() {
        snapshot("tools_list_write_stdio", &write_ctx(Surface::Stdio, WriteLevel::Remote));
    }

    #[test]
    fn snapshot_write_http() {
        snapshot("tools_list_write_http", &write_ctx(Surface::Http, WriteLevel::None));
    }
}
