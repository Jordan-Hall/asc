//! `.asc/ci.toml` (SPEC §12.2): the plan job's switches. A switch is a list of globs over the
//! changed paths, or `{ projects = ["<selector>", ...] }`, true when an affected project matches.
//! `[outputs]` renames a switch's output key. The file comes from a PR, so it is untrusted:
//! its size, the number of switches and globs, and every name are bounded, and a switch name
//! can never take the key of an output the plan writes itself.

use std::collections::BTreeMap;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Largest rules file read.
pub const MAX_RULES_BYTES: usize = 256 * 1024;
const MAX_SWITCHES: usize = 128;
const MAX_GLOBS: usize = 256;
const MAX_SELECTORS: usize = 32;
const MAX_TEXT: usize = 512;

/// Output keys the plan writes itself (`asc ci changed`, `asc ci affected --format gha`).
const RESERVED: &[&str] = &[
    "all",
    "semantic",
    "affected",
    "count",
    "hidden",
    "rust",
    "python",
    "dotnet",
    "node",
    "turbo_filter",
    "nx_projects",
    "matrix",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RulesError {
    #[error("rules: {0}")]
    Invalid(String),
}

fn bad<T>(m: impl Into<String>) -> Result<T, RulesError> {
    Err(RulesError::Invalid(m.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    switches: BTreeMap<String, RawSwitch>,
    #[serde(default)]
    outputs: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSwitch {
    Globs(Vec<String>),
    Projects { projects: Vec<String> },
}

/// What makes a switch true.
#[derive(Debug)]
pub enum Condition {
    /// Any changed path matches.
    Paths(GlobSet),
    /// Any affected project matches one of these selectors.
    Projects(Vec<String>),
}

#[derive(Debug)]
pub struct Switch {
    /// The key written to the output file.
    pub output: String,
    pub when: Condition,
}

/// A parsed `.asc/ci.toml`.
#[derive(Debug, Default)]
pub struct Rules {
    pub switches: BTreeMap<String, Switch>,
}

fn name_ok(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase() || f == '_')
        && s.len() <= 64
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn text_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_TEXT && !s.chars().any(char::is_control)
}

fn check_name(what: &str, n: &str) -> Result<(), RulesError> {
    if !name_ok(n) {
        return bad(format!(
            "{what} '{}' must match [a-z_][a-z0-9_]* (at most 64 bytes)",
            n.escape_debug()
        ));
    }
    if RESERVED.contains(&n) {
        return bad(format!("{what} '{n}' is reserved"));
    }
    Ok(())
}

impl Rules {
    /// Parses and validates the file.
    pub fn parse(bytes: &[u8]) -> Result<Self, RulesError> {
        if bytes.len() > MAX_RULES_BYTES {
            return bad(format!("the file is over {MAX_RULES_BYTES} bytes"));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| RulesError::Invalid("not UTF-8".into()))?;
        // Only the message, not the source excerpt, reaches a CI log.
        let raw: RawFile =
            toml::from_str(text).map_err(|e| RulesError::Invalid(e.message().to_owned()))?;
        if raw.switches.len() > MAX_SWITCHES {
            return bad(format!("more than {MAX_SWITCHES} switches"));
        }
        for (sw, out) in &raw.outputs {
            if !raw.switches.contains_key(sw) {
                return bad(format!(
                    "[outputs] names unknown switch '{}'",
                    sw.escape_debug()
                ));
            }
            check_name("output", out)?;
        }
        let mut used: BTreeMap<&str, &str> = BTreeMap::new();
        let mut switches = BTreeMap::new();
        for (name, sw) in &raw.switches {
            check_name("switch", name)?;
            let output = raw.outputs.get(name).unwrap_or(name);
            if let Some(prev) = used.insert(output, name) {
                return bad(format!(
                    "switches '{prev}' and '{name}' share the output '{output}'"
                ));
            }
            let when = match sw {
                RawSwitch::Globs(globs) => {
                    if globs.len() > MAX_GLOBS {
                        return bad(format!("switch '{name}' has more than {MAX_GLOBS} globs"));
                    }
                    let mut b = GlobSetBuilder::new();
                    for g in globs {
                        if !text_ok(g) {
                            return bad(format!("switch '{name}' has an unusable glob"));
                        }
                        let glob = GlobBuilder::new(g)
                            .literal_separator(true)
                            .backslash_escape(true)
                            .build()
                            .map_err(|e| {
                                RulesError::Invalid(format!("switch '{name}': {}", e.kind()))
                            })?;
                        b.add(glob);
                    }
                    Condition::Paths(b.build().map_err(|e| RulesError::Invalid(e.to_string()))?)
                }
                RawSwitch::Projects { projects } => {
                    if projects.is_empty() || projects.len() > MAX_SELECTORS {
                        return bad(format!(
                            "switch '{name}' needs 1 to {MAX_SELECTORS} selectors"
                        ));
                    }
                    if projects.iter().any(|p| !text_ok(p)) {
                        return bad(format!("switch '{name}' has an unusable selector"));
                    }
                    Condition::Projects(projects.clone())
                }
            };
            switches.insert(
                name.clone(),
                Switch {
                    output: output.clone(),
                    when,
                },
            );
        }
        Ok(Self { switches })
    }

    /// The glob switches over `paths`, as `(output key, value)`. Project switches are left to
    /// `asc ci affected`. When `all` is set every switch is true, project switches included.
    pub fn eval_paths(&self, paths: &[String], all: bool) -> Vec<(String, bool)> {
        self.switches
            .values()
            .filter_map(|s| match &s.when {
                _ if all => Some((s.output.clone(), true)),
                Condition::Paths(set) => Some((
                    s.output.clone(),
                    paths.iter().any(|p| set.is_match(p.as_str())),
                )),
                Condition::Projects(_) => None,
            })
            .collect()
    }

    /// Every switch true (the failure fallback, and a PR that edits the rules).
    pub fn all_true(&self) -> Vec<(String, bool)> {
        self.switches
            .values()
            .map(|s| (s.output.clone(), true))
            .collect()
    }

    /// The project switches with their selectors, as `(output key, selectors)`.
    pub fn project_switches(&self) -> Vec<(&str, &[String])> {
        self.switches
            .values()
            .filter_map(|s| match &s.when {
                Condition::Projects(sel) => Some((s.output.as_str(), sel.as_slice())),
                Condition::Paths(_) => None,
            })
            .collect()
    }
}

/// Whether the repository path `rules` is among the changed `paths`.
pub fn rules_changed(rules: &str, paths: &[String]) -> bool {
    let rules = rules.trim_start_matches("./");
    paths.iter().any(|p| p == rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[switches]
rs = ["apps/analysis-pipeline/**", "**/*.rs", "**/Cargo.toml"]
docs_only = ["docs/**", "*.md"]
images = { projects = ["tag:image"] }
"#;

    fn p(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn rules_evaluate_globs() {
        let r = Rules::parse(SAMPLE.as_bytes()).unwrap();
        let v = r.eval_paths(&p(&["src/lib.rs", "README.md"]), false);
        assert_eq!(v, vec![("docs_only".into(), true), ("rs".into(), true)]);
        let v = r.eval_paths(&p(&["docs/a/b.txt"]), false);
        assert_eq!(v, vec![("docs_only".into(), true), ("rs".into(), false)]);
        // `*` stays inside one path component.
        let v = r.eval_paths(&p(&["a/README.md"]), false);
        assert_eq!(v[0], ("docs_only".into(), false));
    }

    #[test]
    fn project_switches_are_listed_not_evaluated_from_paths() {
        let r = Rules::parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(r.eval_paths(&[], false).len(), 2);
        let ps = r.project_switches();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].0, "images");
        assert_eq!(ps[0].1, ["tag:image".to_owned()]);
        assert_eq!(r.eval_paths(&[], true).len(), 3);
        assert!(r.all_true().iter().all(|(_, v)| *v));
    }

    #[test]
    fn outputs_rename_a_switch() {
        let r = Rules::parse(b"[switches]\na = [\"x\"]\n[outputs]\na = \"run_a\"\n").unwrap();
        assert_eq!(
            r.eval_paths(&p(&["x"]), false),
            vec![("run_a".into(), true)]
        );
    }

    #[test]
    fn hostile_files_are_rejected() {
        for t in [
            "[switches]\nall = [\"x\"]\n",
            "[switches]\n\"a b\" = [\"x\"]\n",
            "[switches]\n\"a\\nb=1\" = [\"x\"]\n",
            "[switches]\nA = [\"x\"]\n",
            "[switches]\na = [\"[\"]\n",
            "[switches]\na = [\"\"]\n",
            "[switches]\na = { projects = [] }\n",
            "[outputs]\nzz = \"y\"\n",
            "[switches]\na = [\"x\"]\n[outputs]\na = \"matrix\"\n",
            "[switches]\na = [\"x\"]\nb = [\"y\"]\n[outputs]\nb = \"a\"\n",
            "turbo = \"x\"\n",
            "[turbo]\nx = 1\n",
        ] {
            assert!(Rules::parse(t.as_bytes()).is_err(), "{t}");
        }
        assert!(Rules::parse(&[0xff, 0xfe]).is_err());
        assert!(Rules::parse(&vec![b'#'; MAX_RULES_BYTES + 1]).is_err());
    }

    #[test]
    fn rules_path_detection() {
        assert!(rules_changed(".asc/ci.toml", &p(&["a", ".asc/ci.toml"])));
        assert!(rules_changed("./.asc/ci.toml", &p(&[".asc/ci.toml"])));
        assert!(!rules_changed(
            ".asc/ci.toml",
            &p(&[".asc/ci.toml.bak", "x/.asc/ci.toml"])
        ));
    }
}
