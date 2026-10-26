//! `asc.mcp.*` configuration and its trust rules (SPEC 22.8, 22.5, M-D4).
//!
//! Configuration arrives in layers. Only the system and user layers may set `asc.mcp.write` and
//! `asc.mcp.confirm`; the repository and worktree layers are ignored for those two keys, because a
//! hostile repository must not widen what an agent may do. Command-line flags (a committed
//! `.mcp.json` can carry any) are applied last and can only narrow: lower the write level, remove
//! toolsets or tools, lower the token cap. `--read-only` overrides everything.

use std::collections::BTreeSet;

use crate::catalogue::{Context, Toolset, WriteLevel};
use crate::schema::Surface;
use crate::shape::{DEFAULT_MAX_TOKENS, HARD_MAX_TOKENS};

/// Where a configuration entry comes from, from most to least trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// System-wide config.
    System,
    /// The user's config.
    User,
    /// The repository's config (committed or shared; untrusted).
    Repo,
    /// The worktree's config (untrusted).
    Worktree,
}

impl Layer {
    /// Whether the layer may set `asc.mcp.write` and `asc.mcp.confirm`.
    #[must_use]
    pub const fn trusted(self) -> bool {
        matches!(self, Self::System | Self::User)
    }
}

/// `asc.mcp.confirm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmMode {
    /// Every confirm-capable call asks.
    Always,
    /// Calls marked destructive ask (the default).
    #[default]
    Destructive,
    /// Nothing asks.
    Never,
}

/// `asc.mcp.textMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextMode {
    /// Git-format text blocks (the default).
    #[default]
    Git,
    /// The text block is the serialized JSON.
    Json,
}

/// One `key = value` entry from a layer; keys are written as in git config, without `asc.mcp.`
/// (for example `write`, `toolsets`, `maxTokens`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The layer.
    pub layer: Layer,
    /// The key after `asc.mcp.`, case-insensitive.
    pub key: String,
    /// The value.
    pub value: String,
}

impl Entry {
    /// An entry.
    #[must_use]
    pub fn new(layer: Layer, key: &str, value: &str) -> Self {
        Self {
            layer,
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }
}

/// Command-line flags of `asc mcp`. Every field can only narrow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    /// `--read-only`.
    pub read_only: bool,
    /// `--write[=local|remote]`.
    pub write: Option<WriteLevel>,
    /// `--toolsets`.
    pub toolsets: Option<BTreeSet<Toolset>>,
    /// `--tools`.
    pub tools: Option<BTreeSet<String>>,
    /// A lower per-call token cap.
    pub max_tokens: Option<u64>,
}

/// The resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// The write level granted by user or system config, narrowed by flags.
    pub write: WriteLevel,
    /// Enabled read toolsets.
    pub toolsets: BTreeSet<Toolset>,
    /// An explicit tool list, intersected with the toolsets and the write gate.
    pub tools: Option<BTreeSet<String>>,
    /// Confirmation mode.
    pub confirm: ConfirmMode,
    /// Per-call token budget, at most [`HARD_MAX_TOKENS`].
    pub max_tokens: u64,
    /// Text block mode.
    pub text_mode: TextMode,
    /// `--read-only`.
    pub read_only: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            write: WriteLevel::None,
            toolsets: BTreeSet::from([Toolset::Core]),
            tools: None,
            confirm: ConfirmMode::Destructive,
            max_tokens: DEFAULT_MAX_TOKENS,
            text_mode: TextMode::Git,
            read_only: false,
        }
    }
}

fn parse_write(v: &str) -> Option<WriteLevel> {
    match v.trim().to_ascii_lowercase().as_str() {
        "false" | "none" | "no" | "off" | "" => Some(WriteLevel::None),
        "local" | "true" => Some(WriteLevel::Local),
        "remote" => Some(WriteLevel::Remote),
        _ => None,
    }
}

fn parse_confirm(v: &str) -> Option<ConfirmMode> {
    match v.trim().to_ascii_lowercase().as_str() {
        "always" => Some(ConfirmMode::Always),
        "destructive" => Some(ConfirmMode::Destructive),
        "never" => Some(ConfirmMode::Never),
        _ => None,
    }
}

/// Parses `asc.mcp.toolsets`: a comma list, or `all`. `write` is accepted and ignored here (the
/// write level controls it). An unknown name is skipped, so a typo narrows rather than widens.
#[must_use]
pub fn parse_toolsets(v: &str) -> BTreeSet<Toolset> {
    let mut out = BTreeSet::new();
    for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if part == "all" {
            out.extend([
                Toolset::Core,
                Toolset::History,
                Toolset::Stack,
                Toolset::Projects,
                Toolset::Search,
            ]);
        } else if let Some(t) = Toolset::parse(part)
            && t != Toolset::Write
        {
            out.insert(t);
        }
    }
    out
}

impl McpConfig {
    /// Resolves layered entries and flags. Within a layer, later entries win; across layers the
    /// order is system, user, repo, worktree for the untrusted keys, and the user layer wins over
    /// the system layer for the trusted keys. Entries of an untrusted layer for `write` and
    /// `confirm` are dropped. Flags narrow last.
    #[must_use]
    pub fn resolve(entries: &[Entry], flags: &Flags) -> Self {
        let mut cfg = Self::default();
        let mut ordered: Vec<&Entry> = entries.iter().collect();
        ordered.sort_by_key(|e| e.layer); // stable: file order is kept within a layer
        for e in ordered {
            match e.key.to_ascii_lowercase().as_str() {
                "write" if e.layer.trusted() => {
                    if let Some(w) = parse_write(&e.value) {
                        cfg.write = w;
                    }
                }
                "confirm" if e.layer.trusted() => {
                    if let Some(c) = parse_confirm(&e.value) {
                        cfg.confirm = c;
                    }
                }
                "toolsets" => cfg.toolsets = parse_toolsets(&e.value),
                "tools" => {
                    cfg.tools = Some(
                        e.value
                            .split(',')
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    );
                }
                "maxtokens" => {
                    if let Ok(n) = e.value.trim().parse::<u64>() {
                        cfg.max_tokens = n.clamp(1, HARD_MAX_TOKENS);
                    }
                }
                "textmode" => match e.value.trim().to_ascii_lowercase().as_str() {
                    "json" => cfg.text_mode = TextMode::Json,
                    "git" => cfg.text_mode = TextMode::Git,
                    _ => {}
                },
                _ => {}
            }
        }
        cfg.apply_flags(flags);
        cfg
    }

    /// Applies flags; each one can only narrow.
    pub fn apply_flags(&mut self, flags: &Flags) {
        if flags.read_only {
            self.read_only = true;
            self.write = WriteLevel::None;
        }
        if let Some(w) = flags.write {
            self.write = self.write.min(w);
        }
        if let Some(t) = &flags.toolsets {
            self.toolsets = self.toolsets.intersection(t).copied().collect();
        }
        if let Some(t) = &flags.tools {
            self.tools = Some(match self.tools.take() {
                Some(cur) => cur.intersection(t).cloned().collect(),
                None => t.clone(),
            });
        }
        if let Some(n) = flags.max_tokens {
            self.max_tokens = self.max_tokens.min(n.max(1));
        }
    }

    /// The catalogue context for a surface. `features` is `Version.features` of the server in
    /// use; `workspace_uploads` says whether drafts upload at once.
    #[must_use]
    pub fn context(
        &self,
        surface: Surface,
        features: &BTreeSet<String>,
        workspace_uploads: bool,
    ) -> Context {
        let mut ctx = Context::new(surface);
        ctx.toolsets = self.toolsets.clone();
        ctx.write = self.write;
        ctx.read_only = self.read_only;
        ctx.workspace_uploads = workspace_uploads;
        ctx.features = features.clone();
        ctx
    }

    /// Whether the explicit `asc.mcp.tools` list (if any) lets `name` through.
    #[must_use]
    pub fn tool_allowed(&self, name: &str) -> bool {
        self.tools.as_ref().is_none_or(|t| t.contains(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(layer: Layer, key: &str, value: &str) -> Entry {
        Entry::new(layer, key, value)
    }

    #[test]
    fn repo_and_worktree_config_cannot_set_write_or_confirm() {
        for layer in [Layer::Repo, Layer::Worktree] {
            let cfg = McpConfig::resolve(
                &[e(layer, "write", "remote"), e(layer, "confirm", "never")],
                &Flags::default(),
            );
            assert_eq!(cfg.write, WriteLevel::None, "{layer:?}");
            assert_eq!(cfg.confirm, ConfirmMode::Destructive, "{layer:?}");
        }
        let cfg = McpConfig::resolve(
            &[
                e(Layer::User, "write", "local"),
                e(Layer::Repo, "write", "remote"),
                e(Layer::Worktree, "confirm", "never"),
            ],
            &Flags::default(),
        );
        assert_eq!(cfg.write, WriteLevel::Local);
        assert_eq!(cfg.confirm, ConfirmMode::Destructive);
        // The trusted layers do set them, and user wins over system.
        let cfg = McpConfig::resolve(
            &[
                e(Layer::User, "write", "local"),
                e(Layer::System, "write", "remote"),
                e(Layer::System, "confirm", "always"),
            ],
            &Flags::default(),
        );
        assert_eq!(
            (cfg.write, cfg.confirm),
            (WriteLevel::Local, ConfirmMode::Always)
        );
    }

    #[test]
    fn untrusted_layers_do_not_reach_the_catalogue() {
        let cfg = McpConfig::resolve(&[e(Layer::Repo, "write", "remote")], &Flags::default());
        let ctx = cfg.context(Surface::Stdio, &BTreeSet::new(), false);
        assert!(
            crate::catalogue::tools(&ctx)
                .iter()
                .all(|l| l.def.toolset != Toolset::Write)
        );
    }

    #[test]
    fn flags_only_narrow() {
        let user = [
            e(Layer::User, "write", "local"),
            e(Layer::User, "toolsets", "core,history"),
            e(Layer::User, "maxTokens", "4000"),
        ];
        let widen = Flags {
            write: Some(WriteLevel::Remote),
            toolsets: Some(BTreeSet::from([Toolset::Core, Toolset::Stack])),
            max_tokens: Some(20_000),
            ..Flags::default()
        };
        let cfg = McpConfig::resolve(&user, &widen);
        assert_eq!(cfg.write, WriteLevel::Local);
        assert_eq!(cfg.toolsets, BTreeSet::from([Toolset::Core]));
        assert_eq!(cfg.max_tokens, 4000);
        let narrow = Flags {
            write: Some(WriteLevel::None),
            ..Flags::default()
        };
        assert_eq!(McpConfig::resolve(&user, &narrow).write, WriteLevel::None);
        // A flag cannot grant write when config grants none.
        let cfg = McpConfig::resolve(
            &[],
            &Flags {
                write: Some(WriteLevel::Remote),
                ..Flags::default()
            },
        );
        assert_eq!(cfg.write, WriteLevel::None);
    }

    #[test]
    fn read_only_wins_over_everything() {
        let user = [
            e(Layer::System, "write", "remote"),
            e(Layer::User, "write", "remote"),
        ];
        let cfg = McpConfig::resolve(
            &user,
            &Flags {
                read_only: true,
                write: Some(WriteLevel::Remote),
                ..Flags::default()
            },
        );
        assert!(cfg.read_only);
        assert_eq!(cfg.write, WriteLevel::None);
        let ctx = cfg.context(Surface::Stdio, &BTreeSet::new(), false);
        assert!(
            crate::catalogue::tools(&ctx)
                .iter()
                .all(|l| l.def.toolset != Toolset::Write)
        );
    }

    #[test]
    fn tools_list_is_intersected_and_max_tokens_is_capped() {
        let cfg = McpConfig::resolve(
            &[
                e(Layer::User, "tools", "log,diff"),
                e(Layer::User, "maxTokens", "999999"),
            ],
            &Flags {
                tools: Some(BTreeSet::from(["log".to_owned(), "grep".to_owned()])),
                ..Flags::default()
            },
        );
        assert!(cfg.tool_allowed("log") && !cfg.tool_allowed("diff") && !cfg.tool_allowed("grep"));
        assert_eq!(cfg.max_tokens, HARD_MAX_TOKENS);
    }

    #[test]
    fn toolsets_parse_all_and_ignore_unknown() {
        assert_eq!(parse_toolsets("all").len(), 5);
        assert_eq!(
            parse_toolsets("core, nope ,write"),
            BTreeSet::from([Toolset::Core])
        );
    }
}
