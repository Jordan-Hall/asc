//! `.asc/projects.toml` (SPEC §12A.3): parser with line-numbered errors and
//! named-input resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use asc_core::path::RepoPath;
use serde::{Deserialize, Serialize};
use toml::Spanned;
use toml::de::{DeTable, DeValue};

use crate::model::Ecosystem;
use crate::pattern::InputItem;
use crate::select::Selector;

/// A config error naming the 1-based line it comes from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ConfigError {
    pub line: u32,
    pub message: String,
}

/// Why named inputs could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("unknown named input {0:?}")]
    Unknown(String),
    #[error("named inputs form a cycle: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
    #[error("named inputs expand to more than {MAX_EXPANDED_ITEMS} items")]
    TooLarge,
}

/// Most input items visited while resolving one list.
pub const MAX_EXPANDED_ITEMS: usize = 100_000;

/// What a changed path with no owner does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unowned {
    /// Nothing; the path is only reported.
    #[default]
    Ignore,
    /// Every project is affected.
    All,
}

/// `[ecosystem.<e>]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EcoConfig {
    pub enabled: bool,
    /// Extra eco-global input globs.
    pub inputs: Vec<String>,
}

impl Default for EcoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            inputs: Vec::new(),
        }
    }
}

/// What a `[[project]]` entry applies to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectTarget {
    /// Overrides the one detected project `select` matches.
    Select(Selector),
    /// Declares a project (SPEC §12A.2.5); `root_line` locates the `root` value.
    Declared {
        name: String,
        root: RepoPath,
        root_line: u32,
    },
}

/// One `[[project]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Line of the `select` or `name` value, for link-time errors.
    pub line: u32,
    pub target: ProjectTarget,
    /// Per-target input lists.
    pub inputs: BTreeMap<String, Vec<InputItem>>,
    /// Selectors that add `Implicit` edges, or remove edges when excluded (`!`).
    pub implicit_dependencies: Vec<Selector>,
    pub tags: Vec<String>,
    /// `argv = { <target> = [...] }`: the command an IDE runs for a target (SPEC §24.8.2, A4).
    /// Validated by [`crate::targets::check_argv`]; asc stores and serves it and never runs it.
    pub argv: BTreeMap<String, Vec<String>>,
}

/// The validated contents of `.asc/projects.toml`; `Default` is an absent file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedConfig {
    pub ignore: Vec<String>,
    pub global_inputs: Vec<String>,
    pub unowned: Unowned,
    pub builtin_eco_globals: bool,
    pub ecosystems: BTreeMap<Ecosystem, EcoConfig>,
    /// User-defined named inputs; `default` is built in unless redefined.
    pub named_inputs: BTreeMap<String, Vec<InputItem>>,
    pub targets: BTreeMap<String, Vec<InputItem>>,
    pub projects: Vec<ProjectEntry>,
}

impl Default for ResolvedConfig {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            global_inputs: Vec::new(),
            unowned: Unowned::Ignore,
            builtin_eco_globals: true,
            ecosystems: BTreeMap::new(),
            named_inputs: BTreeMap::new(),
            targets: BTreeMap::new(),
            projects: Vec::new(),
        }
    }
}

/// Flattened input list: globs plus the named inputs to read from dependencies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedInputs {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    /// Names of the `^name` items.
    pub deps: Vec<String>,
}

/// Expansion state of one `resolve` call.
struct Expand<'a> {
    stack: Vec<&'a str>,
    /// Names fully expanded already.
    done: BTreeSet<&'a str>,
    budget: usize,
}

const DEFAULT_INPUT: &str = "default";
const DEFAULT_GLOB: &str = "{projectRoot}/**";

impl ResolvedConfig {
    /// The ecosystem's settings, or the defaults.
    pub fn eco(&self, eco: Ecosystem) -> EcoConfig {
        self.ecosystems.get(&eco).cloned().unwrap_or_default()
    }

    /// Items for `target` of a project: its own entry, then `[targets.<t>]`,
    /// then the built-in `["default", "^default"]`. Without a target: `["default"]`.
    pub fn target_items(
        &self,
        entry: Option<&ProjectEntry>,
        target: Option<&str>,
    ) -> Vec<InputItem> {
        let default = || InputItem::Named(DEFAULT_INPUT.to_owned());
        let Some(t) = target else {
            return vec![default()];
        };
        entry
            .and_then(|e| e.inputs.get(t))
            .or_else(|| self.targets.get(t))
            .cloned()
            .unwrap_or_else(|| vec![default(), InputItem::Dep(DEFAULT_INPUT.to_owned())])
    }

    /// Resolved inputs for `target` of a project.
    pub fn inputs_for(
        &self,
        entry: Option<&ProjectEntry>,
        target: Option<&str>,
    ) -> Result<ResolvedInputs, ResolveError> {
        self.resolve(&self.target_items(entry, target))
    }

    /// Expands named items recursively into globs; `^name` items are collected.
    pub fn resolve(&self, items: &[InputItem]) -> Result<ResolvedInputs, ResolveError> {
        let mut out = ResolvedInputs::default();
        let mut st = Expand {
            stack: Vec::new(),
            done: BTreeSet::new(),
            budget: MAX_EXPANDED_ITEMS,
        };
        self.expand(items, &mut st, &mut out)?;
        Ok(out)
    }

    fn expand<'a>(
        &'a self,
        items: &'a [InputItem],
        st: &mut Expand<'a>,
        out: &mut ResolvedInputs,
    ) -> Result<(), ResolveError> {
        fn push(v: &mut Vec<String>, s: &str) {
            if !v.iter().any(|x| x == s) {
                v.push(s.to_owned());
            }
        }
        for item in items {
            st.budget = st.budget.checked_sub(1).ok_or(ResolveError::TooLarge)?;
            match item {
                InputItem::Include(g) => push(&mut out.includes, g),
                InputItem::Exclude(g) => push(&mut out.excludes, g),
                InputItem::Dep(n) => {
                    self.check_name(n)?;
                    push(&mut out.deps, n);
                }
                InputItem::Named(n) => {
                    if let Some(i) = st.stack.iter().position(|s| s == n) {
                        let mut cycle: Vec<String> =
                            st.stack[i..].iter().map(|s| (*s).to_owned()).collect();
                        cycle.push(n.clone());
                        return Err(ResolveError::Cycle(cycle));
                    }
                    // The output is deduplicated, so a finished name adds nothing.
                    if st.done.contains(n.as_str()) {
                        continue;
                    }
                    st.stack.push(n);
                    match self.named_inputs.get(n) {
                        Some(inner) => self.expand(inner, st, out)?,
                        None if n == DEFAULT_INPUT => push(&mut out.includes, DEFAULT_GLOB),
                        None => return Err(ResolveError::Unknown(n.clone())),
                    }
                    st.stack.pop();
                    st.done.insert(n);
                }
            }
        }
        Ok(())
    }

    fn check_name(&self, n: &str) -> Result<(), ResolveError> {
        if n == DEFAULT_INPUT || self.named_inputs.contains_key(n) {
            Ok(())
        } else {
            Err(ResolveError::Unknown(n.to_owned()))
        }
    }
}

/// Parses and validates a `.asc/projects.toml`.
///
/// Link-time checks (a `select` that matches exactly one project, a declared
/// `root` that exists) stay with the caller, which reports `ProjectEntry::line`.
pub fn parse(text: &str) -> Result<ResolvedConfig, ConfigError> {
    let doc = DeTable::parse(text).map_err(|e| {
        let line = e.span().map_or(1, |s| line_of(text, s.start));
        ConfigError {
            line,
            message: e.message().to_owned(),
        }
    })?;
    let mut p = Parser {
        text,
        cfg: ResolvedConfig::default(),
        defs: BTreeMap::new(),
        refs: Vec::new(),
    };
    p.root(doc.get_ref())?;
    p.validate()?;
    Ok(p.cfg)
}

fn line_of(text: &str, offset: usize) -> u32 {
    let end = offset.min(text.len());
    let n = text.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count();
    u32::try_from(n + 1).unwrap_or(u32::MAX)
}

type Val<'i> = Spanned<DeValue<'i>>;

struct Parser<'a> {
    text: &'a str,
    cfg: ResolvedConfig,
    /// Line of each user-defined named input.
    defs: BTreeMap<String, u32>,
    /// Every named-input reference with its line.
    refs: Vec<(String, u32)>,
}

impl Parser<'_> {
    fn err<T>(&self, span: Range<usize>, message: impl Into<String>) -> Result<T, ConfigError> {
        Err(ConfigError {
            line: line_of(self.text, span.start),
            message: message.into(),
        })
    }

    fn table<'t, 'i>(&self, v: &'t Val<'i>, what: &str) -> Result<&'t DeTable<'i>, ConfigError> {
        match v.get_ref() {
            DeValue::Table(t) => Ok(t),
            _ => self.err(v.span(), format!("{what} must be a table")),
        }
    }

    fn string(&self, v: &Val<'_>, what: &str) -> Result<String, ConfigError> {
        match v.get_ref() {
            DeValue::String(s) => Ok(s.as_ref().to_owned()),
            _ => self.err(v.span(), format!("{what} must be a string")),
        }
    }

    fn boolean(&self, v: &Val<'_>, what: &str) -> Result<bool, ConfigError> {
        v.get_ref().as_bool().map_or_else(
            || self.err(v.span(), format!("{what} must be a boolean")),
            Ok,
        )
    }

    /// The elements of a string array, each with its span.
    fn strings(&self, v: &Val<'_>, what: &str) -> Result<Vec<(String, Range<usize>)>, ConfigError> {
        let DeValue::Array(a) = v.get_ref() else {
            return self.err(v.span(), format!("{what} must be an array of strings"));
        };
        a.iter()
            .map(|e| Ok((self.string(e, &format!("{what} item"))?, e.span())))
            .collect()
    }

    fn string_list(&self, v: &Val<'_>, what: &str) -> Result<Vec<String>, ConfigError> {
        Ok(self.strings(v, what)?.into_iter().map(|(s, _)| s).collect())
    }

    fn globs(&self, v: &Val<'_>, what: &str) -> Result<Vec<String>, ConfigError> {
        let list = self.strings(v, what)?;
        for (g, span) in &list {
            if let Err(e) = InputItem::parse(g) {
                return self.err(span.clone(), e.to_string());
            }
        }
        Ok(list.into_iter().map(|(g, _)| g).collect())
    }

    /// An input list; records its named-input references.
    fn items(&mut self, v: &Val<'_>, what: &str) -> Result<Vec<InputItem>, ConfigError> {
        let mut out = Vec::new();
        for (s, span) in self.strings(v, what)? {
            let item = match InputItem::parse(&s) {
                Ok(i) => i,
                Err(e) => return self.err(span, e.to_string()),
            };
            if let InputItem::Named(n) | InputItem::Dep(n) = &item {
                self.refs.push((n.clone(), line_of(self.text, span.start)));
            }
            out.push(item);
        }
        Ok(out)
    }

    fn root(&mut self, t: &DeTable<'_>) -> Result<(), ConfigError> {
        for (k, v) in t {
            let key = k.get_ref().as_ref();
            match key {
                "version" => match v.get_ref().as_integer() {
                    Some(i) if i.as_str() == "1" => {}
                    _ => return self.err(v.span(), "unsupported version (expected 1)"),
                },
                "ignore" => self.cfg.ignore = self.globs(v, "ignore")?,
                "global" => self.global(v)?,
                "ecosystem" => self.ecosystems(v)?,
                "named_inputs" => {
                    for (name, list) in self.table(v, "named_inputs")? {
                        let items = self.items(list, "named input")?;
                        let name_s = name.get_ref().as_ref();
                        self.defs
                            .insert(name_s.to_owned(), line_of(self.text, name.span().start));
                        self.cfg.named_inputs.insert(name_s.to_owned(), items);
                    }
                }
                "targets" => {
                    for (name, tv) in self.table(v, "targets")? {
                        let tt = self.table(tv, "target")?;
                        let mut inputs = None;
                        for (tk, tval) in tt {
                            match tk.get_ref().as_ref() {
                                "inputs" => inputs = Some(self.items(tval, "inputs")?),
                                other => {
                                    return self.err(tk.span(), format!("unknown key {other:?}"));
                                }
                            }
                        }
                        if let Some(items) = inputs {
                            self.cfg
                                .targets
                                .insert(name.get_ref().as_ref().to_owned(), items);
                        }
                    }
                }
                "project" => {
                    let DeValue::Array(a) = v.get_ref() else {
                        return self
                            .err(v.span(), "project must be an array of tables ([[project]])");
                    };
                    for e in a {
                        let entry = self.project(e)?;
                        self.cfg.projects.push(entry);
                    }
                }
                other => return self.err(k.span(), format!("unknown key {other:?}")),
            }
        }
        Ok(())
    }

    fn global(&mut self, v: &Val<'_>) -> Result<(), ConfigError> {
        for (k, val) in self.table(v, "global")? {
            match k.get_ref().as_ref() {
                "inputs" => self.cfg.global_inputs = self.globs(val, "inputs")?,
                "unowned" => {
                    self.cfg.unowned = match self.string(val, "unowned")?.as_str() {
                        "ignore" => Unowned::Ignore,
                        "all" => Unowned::All,
                        _ => return self.err(val.span(), "unowned must be \"ignore\" or \"all\""),
                    }
                }
                "builtin_eco_globals" => {
                    self.cfg.builtin_eco_globals = self.boolean(val, "builtin_eco_globals")?
                }
                other => return self.err(k.span(), format!("unknown key {other:?}")),
            }
        }
        Ok(())
    }

    fn ecosystems(&mut self, v: &Val<'_>) -> Result<(), ConfigError> {
        for (name, ev) in self.table(v, "ecosystem")? {
            let eco = match Ecosystem::parse(name.get_ref().as_ref()) {
                Some(e) if e != Ecosystem::Declared => e,
                _ => {
                    return self.err(
                        name.span(),
                        format!("unknown ecosystem {:?}", name.get_ref().as_ref()),
                    );
                }
            };
            let mut cfg = EcoConfig::default();
            for (k, val) in self.table(ev, "ecosystem")? {
                match k.get_ref().as_ref() {
                    "enabled" => cfg.enabled = self.boolean(val, "enabled")?,
                    "inputs" => cfg.inputs = self.globs(val, "inputs")?,
                    other => return self.err(k.span(), format!("unknown key {other:?}")),
                }
            }
            self.cfg.ecosystems.insert(eco, cfg);
        }
        Ok(())
    }

    fn project(&mut self, v: &Val<'_>) -> Result<ProjectEntry, ConfigError> {
        let t = self.table(v, "project")?;
        let (mut select, mut name, mut root) = (None, None, None);
        let (mut inputs, mut implicit, mut tags) = (BTreeMap::new(), Vec::new(), Vec::new());
        let mut argv = BTreeMap::new();
        for (k, val) in t {
            match k.get_ref().as_ref() {
                "select" => select = Some((self.string(val, "select")?, val.span())),
                "name" => name = Some((self.string(val, "name")?, val.span())),
                "root" => root = Some((self.string(val, "root")?, val.span())),
                "tags" => tags = self.string_list(val, "tags")?,
                "argv" => {
                    for (target, list) in self.table(val, "argv")? {
                        let name = target.get_ref().as_ref().to_owned();
                        let words = self.string_list(list, "argv")?;
                        if let Err(m) = crate::targets::check_argv(&name, &words) {
                            return self.err(list.span(), m);
                        }
                        argv.insert(name, words);
                    }
                    if let Err(m) = crate::targets::check_table(&argv) {
                        return self.err(val.span(), m);
                    }
                }
                "inputs" => {
                    for (target, list) in self.table(val, "inputs")? {
                        let items = self.items(list, "inputs")?;
                        inputs.insert(target.get_ref().as_ref().to_owned(), items);
                    }
                }
                "implicit_dependencies" => {
                    for (s, span) in self.strings(val, "implicit_dependencies")? {
                        match Selector::parse(&s) {
                            Ok(sel) => implicit.push(sel),
                            Err(e) => return self.err(span, e.to_string()),
                        }
                    }
                }
                other => return self.err(k.span(), format!("unknown key {other:?}")),
            }
        }
        let (line, target) = match (select, name, root) {
            (Some((s, span)), None, None) => match Selector::parse(&s) {
                Ok(sel) if sel.exclude => return self.err(span, "select cannot be an exclusion"),
                Ok(sel) => (line_of(self.text, span.start), ProjectTarget::Select(sel)),
                Err(e) => return self.err(span, e.to_string()),
            },
            (None, Some((name, nspan)), Some((root, rspan))) => {
                let path = RepoPath::new(root.trim_end_matches('/'))
                    .or_else(|e| self.err(rspan.clone(), format!("invalid root: {e}")))?;
                let target = ProjectTarget::Declared {
                    name,
                    root: path,
                    root_line: line_of(self.text, rspan.start),
                };
                (line_of(self.text, nspan.start), target)
            }
            _ => {
                return self.err(
                    v.span(),
                    "a project entry needs either `select`, or both `name` and `root`",
                );
            }
        };
        Ok(ProjectEntry {
            line,
            target,
            inputs,
            implicit_dependencies: implicit,
            tags,
            argv,
        })
    }

    /// Unknown references and cycles among named inputs.
    fn validate(&self) -> Result<(), ConfigError> {
        for (name, line) in &self.refs {
            if name != DEFAULT_INPUT && !self.defs.contains_key(name) {
                return Err(ConfigError {
                    line: *line,
                    message: format!("unknown named input {name:?}"),
                });
            }
        }
        for (name, line) in &self.defs {
            let items = [InputItem::Named(name.clone())];
            if let Err(ResolveError::Cycle(path)) = self.cfg.resolve(&items) {
                // The definition that closes the loop holds the offending reference.
                let closing = path
                    .get(path.len().wrapping_sub(2))
                    .and_then(|n| self.defs.get(n));
                return Err(ConfigError {
                    line: *closing.unwrap_or(line),
                    message: ResolveError::Cycle(path).to_string(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPEC §12A.3 example.
    const SPEC_EXAMPLE: &str = r#"version = 1
ignore = ["**/fixtures/**", "examples/**"]

[global]
inputs = [".forgejo/workflows/ci.yml"]
unowned = "ignore"
builtin_eco_globals = true

[ecosystem.node]
inputs = [".env.example"]
[ecosystem.dotnet]
enabled = false

[named_inputs]
production = ["default", "!{projectRoot}/**/*.md", "!{projectRoot}/**/*.test.ts", "!{projectRoot}/tests/**"]

[targets.build]
inputs = ["production", "^production"]
[targets.test]
inputs = ["default", "^production"]

[[project]]
select = "node:@asc/plugin-runtime"
inputs = { test = ["default", "^production", "{workspaceRoot}/schemas/**"] }
implicit_dependencies = ["rust:dsl-napi", "!node:@asc/ui"]
tags = ["image", "deploy"]

[[project]]
name = "migrations"
root = "packages/database/migrations"
tags = ["db"]
"#;

    fn fail(text: &str) -> ConfigError {
        parse(text).unwrap_err()
    }

    #[test]
    fn parses_spec_example() {
        let c = parse(SPEC_EXAMPLE).unwrap();
        assert_eq!(c.ignore, ["**/fixtures/**", "examples/**"]);
        assert_eq!(c.global_inputs, [".forgejo/workflows/ci.yml"]);
        assert_eq!(c.unowned, Unowned::Ignore);
        assert!(c.builtin_eco_globals);
        assert_eq!(c.eco(Ecosystem::Node).inputs, [".env.example"]);
        assert!(!c.eco(Ecosystem::Dotnet).enabled);
        assert!(c.eco(Ecosystem::Rust).enabled);
        assert_eq!(c.projects.len(), 2);
        let ProjectTarget::Select(s) = &c.projects[0].target else {
            panic!()
        };
        assert_eq!(
            s.term,
            crate::select::Term::Id {
                eco: Ecosystem::Node,
                name: "@asc/plugin-runtime".into(),
                root: None
            }
        );
        assert_eq!(c.projects[0].line, 23);
        assert_eq!(c.projects[0].tags, ["image", "deploy"]);
        assert!(c.projects[0].implicit_dependencies[1].exclude);
        let ProjectTarget::Declared {
            name,
            root,
            root_line,
        } = &c.projects[1].target
        else {
            panic!()
        };
        assert_eq!(
            (name.as_str(), root.to_string().as_str(), *root_line),
            ("migrations", "packages/database/migrations", 30)
        );
    }

    #[test]
    fn named_input_resolution() {
        let c = parse(SPEC_EXAMPLE).unwrap();
        let build = c.inputs_for(None, Some("build")).unwrap();
        assert_eq!(build.includes, ["{projectRoot}/**"]);
        assert_eq!(
            build.excludes,
            [
                "{projectRoot}/**/*.md",
                "{projectRoot}/**/*.test.ts",
                "{projectRoot}/tests/**"
            ]
        );
        assert_eq!(build.deps, ["production"]);
        // The project entry overrides `[targets.test]`.
        let own = c.inputs_for(Some(&c.projects[0]), Some("test")).unwrap();
        assert_eq!(
            own.includes,
            ["{projectRoot}/**", "{workspaceRoot}/schemas/**"]
        );
        let test = c.inputs_for(None, Some("test")).unwrap();
        assert_eq!(test.includes, ["{projectRoot}/**"]);
        // Built-ins: no target -> default; unconfigured target -> default + ^default.
        let none = c.inputs_for(None, None).unwrap();
        assert_eq!((none.includes.len(), none.deps.len()), (1, 0));
        let lint = c.inputs_for(None, Some("lint")).unwrap();
        assert_eq!(lint.deps, ["default"]);
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(parse("").unwrap(), ResolvedConfig::default());
        let d = ResolvedConfig::default();
        assert_eq!(
            d.inputs_for(None, Some("build")).unwrap().includes,
            ["{projectRoot}/**"]
        );
    }

    #[test]
    fn syntax_error_has_line() {
        assert_eq!(fail("version = 1\nignore = [\n").line, 2);
        assert_eq!(fail("a = 1\nb = = 2").line, 2);
    }

    #[test]
    fn unknown_keys_name_the_line() {
        assert_eq!(fail("version = 1\nbogus = 1\n").line, 2);
        assert_eq!(fail("[global]\ninputs = []\nwhat = 1\n").line, 3);
        assert_eq!(fail("[ecosystem.go]\nenabled = true\n").line, 1);
        assert_eq!(fail("[ecosystem.node]\n\nfoo = 1\n").line, 3);
        assert_eq!(fail("[[project]]\nselect = \"a\"\nnope = 1\n").line, 3);
        assert_eq!(fail("[targets.build]\nfoo = []\n").line, 2);
    }

    #[test]
    fn type_and_value_errors() {
        assert_eq!(fail("version = 2").line, 1);
        assert_eq!(fail("\n[global]\nunowned = \"some\"\n").line, 3);
        assert_eq!(fail("ignore = [\"ok\", 5]\n").line, 1);
        assert_eq!(fail("ignore = [\"ok\",\n \"a/[b\"]\n").line, 2);
        assert_eq!(fail("[named_inputs]\nx = [\"!y\"]\n").line, 2);
    }

    #[test]
    fn unknown_named_input_names_the_line() {
        let e = fail("[named_inputs]\na = [\"default\"]\nb = [\n  \"a\",\n  \"missing\",\n]\n");
        assert_eq!(e.line, 5);
        assert!(e.message.contains("missing"));
        assert_eq!(fail("[targets.t]\ninputs = [\"^nope\"]\n").line, 2);
    }

    #[test]
    fn named_input_cycle_is_an_error() {
        let e = fail("[named_inputs]\na = [\"b\"]\nb = [\"c\"]\nc = [\"a\"]\n");
        assert_eq!(e.line, 4);
        assert!(e.message.contains("a -> b -> c -> a"), "{e}");
        let selfref = fail("[named_inputs]\nx = [\"x\"]\n");
        assert_eq!(selfref.line, 2);
        // `^x` reads a dependency's input and is not a cycle.
        assert!(parse("[named_inputs]\nx = [\"default\", \"^x\"]\n").is_ok());
        // Redefining the built-in `default` is allowed, and can cycle too.
        assert_eq!(
            fail("[named_inputs]\ndefault = [\"p\"]\np = [\"default\"]\n").line,
            3
        );
    }

    #[test]
    fn project_entry_shapes() {
        assert_eq!(fail("[[project]]\ntags = []\n").line, 1);
        assert_eq!(
            fail("[[project]]\nselect = \"a\"\nname = \"n\"\nroot = \"r\"\n").line,
            1
        );
        assert_eq!(fail("[[project]]\nname = \"n\"\n").line, 1);
        assert_eq!(fail("[[project]]\nselect = \"!tag:x\"\n").line, 2);
        assert_eq!(fail("[[project]]\nname = \"n\"\nroot = \"/abs\"\n").line, 3);
        assert_eq!(
            fail("[[project]]\nselect = \"a\"\nimplicit_dependencies = [\"eco:go\"]\n").line,
            3
        );
    }

    #[test]
    fn diamond_chain_is_linear() {
        let mut c = ResolvedConfig::default();
        for i in 0..64 {
            let next = InputItem::Named(format!("n{}", i + 1));
            c.named_inputs
                .insert(format!("n{i}"), vec![next.clone(), next]);
        }
        c.named_inputs
            .insert("n64".into(), vec![InputItem::Include("a/**".into())]);
        let r = c.resolve(&[InputItem::Named("n0".into())]).unwrap();
        assert_eq!(r.includes, ["a/**"]);
        // Order of first occurrence is kept for ordinary lists.
        c.named_inputs.insert(
            "x".into(),
            vec![InputItem::Include("1".into()), InputItem::Named("y".into())],
        );
        c.named_inputs.insert(
            "y".into(),
            vec![
                InputItem::Include("2".into()),
                InputItem::Include("1".into()),
            ],
        );
        let r = c
            .resolve(&[InputItem::Named("x".into()), InputItem::Named("y".into())])
            .unwrap();
        assert_eq!(r.includes, ["1", "2"]);
    }

    #[test]
    fn expansion_is_capped() {
        let mut c = ResolvedConfig::default();
        let big = vec![InputItem::Include("g".into()); MAX_EXPANDED_ITEMS + 1];
        c.named_inputs.insert("big".into(), big);
        assert_eq!(
            c.resolve(&[InputItem::Named("big".into())]),
            Err(ResolveError::TooLarge)
        );
    }

    #[test]
    fn resolve_reports_cycles_for_programmatic_configs() {
        let mut c = ResolvedConfig::default();
        c.named_inputs
            .insert("a".into(), vec![InputItem::Named("b".into())]);
        c.named_inputs
            .insert("b".into(), vec![InputItem::Named("a".into())]);
        assert_eq!(
            c.resolve(&[InputItem::Named("a".into())]),
            Err(ResolveError::Cycle(vec![
                "a".into(),
                "b".into(),
                "a".into()
            ]))
        );
        assert_eq!(
            c.resolve(&[InputItem::Named("zzz".into())]),
            Err(ResolveError::Unknown("zzz".into()))
        );
    }
}
