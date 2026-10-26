//! Repository policy as code: the schema, the strict parser and the pure decisions of
//! `.asc/policy.toml`.
//!
//! A repository may carry [`POLICY_PATH`] on its trunk. The file the server enforces is the one
//! on the trunk tip *before* an update, never the one in the update, so a change cannot lift the
//! rules that bind it. This module knows nothing about servers, stores or principals' wire forms:
//! it parses and validates the file, and answers "may this [`Who`] write this path" and "does
//! this run fit the agent budget". The enforcement point is `asc-serve`'s ref policy.
//!
//! ```toml
//! version = 1
//!
//! [roles]                      # who the roles name; `maintainer` is required and non-empty
//! maintainer = ["user:alice", "user:bob"]
//!
//! [trunk]                      # the default branch
//! require_approvals = 1        # every change landing on trunk needs this many approvals
//! require_checks = ["build"]   # and a successful run of each named check on its version
//! allow_force = false          # no non-fast-forward update and no deletion of trunk
//!
//! [paths."src/**"]             # every matching rule must admit the writer
//! write = ["role:maintainer", "agent:*"]
//!
//! [agents]
//! default_scope = ["src/**", "tests/**"]   # an agent writes only here, or where a rule names it
//! denied_paths = [".github/**"]            # an agent never writes here
//! [agents.budgets]
//! max_changes_per_run = 20     # a run is one ref-update request
//! max_paths_per_run = 200
//!
//! [merge]
//! strategy = "rebase"          # rebase | rebase-disjoint | fast-forward
//! ```
//!
//! # Subjects
//!
//! A subject is `*` (anyone), `<kind>:<glob>` or `role:<name>`. The kinds are `user`, `agent`,
//! `deploy`, `ci` and `host`; the glob has `*` for any run of characters. A role holds subjects
//! of the first two forms only, so a role never refers to another role.
//!
//! # Paths
//!
//! A path pattern is a `/`-separated glob: `*` and `?` match inside one segment, a `**` segment
//! matches any number of segments (none included, so `src/**` also
//! names a file `src`). Matching ignores ASCII case, so a rule holds
//! on a case-insensitive checkout too. Changing [`POLICY_PATH`] always needs a maintainer,
//! whatever the rules say.
//!
//! # Bounds
//!
//! The file is at most [`MAX_POLICY_BYTES`]; unknown keys are an error at every level; the number
//! and length of every list and name is capped below. An invalid file is an error, never a
//! partial policy.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Where the policy lives in a repository.
pub const POLICY_PATH: &str = ".asc/policy.toml";
/// The one schema version this code reads.
pub const POLICY_VERSION: u32 = 1;
/// Largest policy file.
pub const MAX_POLICY_BYTES: usize = 64 * 1024;
/// Most roles, subjects per list, path rules, globs per list and required checks.
pub const MAX_ROLES: usize = 16;
pub const MAX_SUBJECTS: usize = 64;
pub const MAX_PATH_RULES: usize = 128;
pub const MAX_GLOBS: usize = 64;
pub const MAX_CHECKS: usize = 32;
/// Longest glob, subject, role name and check name, in bytes.
pub const MAX_GLOB_LEN: usize = 256;
pub const MAX_SUBJECT_LEN: usize = 128;
pub const MAX_NAME_LEN: usize = 128;
/// Most approvals a policy can require, and the largest budget number.
pub const MAX_APPROVALS: u32 = 16;
pub const MAX_BUDGET: u32 = 100_000;
/// The role that may change the policy.
pub const MAINTAINER: &str = "maintainer";

/// Why a policy file was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PolicyError(pub String);

fn bad<T>(m: impl Into<String>) -> Result<T, PolicyError> {
    Err(PolicyError(m.into()))
}

// ------------------------------------------------------------------------------ who

/// The kind of a principal, as far as a policy distinguishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A person, by personal token, SSH key or an identity provider.
    User,
    /// An autonomous agent with a credential of its own.
    Agent,
    /// A deploy key or deploy token: it acts for its repository.
    Deploy,
    /// A pipeline or Actions task.
    Ci,
    /// The host application acting on its own behalf.
    Host,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Deploy => "deploy",
            Self::Ci => "ci",
            Self::Host => "host",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "agent" => Self::Agent,
            "deploy" => Self::Deploy,
            "ci" => Self::Ci,
            "host" => Self::Host,
            _ => return None,
        })
    }
}

/// A caller as the policy sees it. `None` is a principal of no known kind (anonymous, internal
/// workers): it matches `*` only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Who {
    pub kind: Option<Kind>,
    pub name: String,
}

impl Who {
    pub fn new(kind: Kind, name: impl Into<String>) -> Self {
        Self {
            kind: Some(kind),
            name: name.into(),
        }
    }

    /// A principal of no known kind.
    pub fn unknown() -> Self {
        Self {
            kind: None,
            name: String::new(),
        }
    }

    pub fn is_agent(&self) -> bool {
        self.kind == Some(Kind::Agent)
    }
}

// ------------------------------------------------------------------------------ subjects

/// Who a rule names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// `*`.
    Anyone,
    /// `<kind>:<glob>`.
    Of { kind: Kind, glob: String },
    /// `role:<name>`.
    Role(String),
}

impl Subject {
    fn parse(text: &str, allow_role: bool) -> Result<Self, PolicyError> {
        if text.is_empty() || text.len() > MAX_SUBJECT_LEN {
            return bad("a subject is empty or too long");
        }
        if text == "*" {
            return if allow_role {
                Ok(Self::Anyone)
            } else {
                bad("a role cannot be everyone")
            };
        }
        let Some((head, rest)) = text.split_once(':') else {
            return bad(format!(
                "subject {text:?} is not `*`, `<kind>:<name>` or `role:<name>`"
            ));
        };
        if head == "role" {
            if !allow_role {
                return bad("a role cannot refer to another role");
            }
            valid_role_name(rest)?;
            return Ok(Self::Role(rest.to_owned()));
        }
        let Some(kind) = Kind::parse(head) else {
            return bad(format!("unknown subject kind {head:?}"));
        };
        if rest.is_empty()
            || !rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._@+-/*".contains(&b))
        {
            return bad(format!("subject {text:?} has an empty or unusual name"));
        }
        Ok(Self::Of {
            kind,
            glob: rest.to_owned(),
        })
    }

    /// The subject as written.
    pub fn render(&self) -> String {
        match self {
            Self::Anyone => "*".to_owned(),
            Self::Of { kind, glob } => format!("{}:{glob}", kind.as_str()),
            Self::Role(r) => format!("role:{r}"),
        }
    }
}

fn valid_role_name(n: &str) -> Result<(), PolicyError> {
    let ok = !n.is_empty()
        && n.len() <= 32
        && n.as_bytes()[0].is_ascii_lowercase()
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        bad(format!(
            "role name {n:?} is not lower-case letters, digits, - and _"
        ))
    }
}

/// `*` matches any run of bytes (slashes included); everything else is literal.
fn name_glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ------------------------------------------------------------------------------ path globs

/// A validated path pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathGlob {
    text: String,
    segments: Vec<Seg>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    /// `**`.
    Deep,
    /// A segment with `*` and `?` wildcards, lower-cased.
    One(Vec<u8>),
}

impl PathGlob {
    pub fn parse(text: &str) -> Result<Self, PolicyError> {
        if text.is_empty() || text.len() > MAX_GLOB_LEN {
            return bad("a path pattern is empty or too long");
        }
        if text.starts_with('/') || text.ends_with('/') || text.contains('\\') {
            return bad(format!(
                "path pattern {text:?} must be relative, with `/` separators and no trailing `/`"
            ));
        }
        if text.chars().any(char::is_control) {
            return bad("a path pattern holds a control character");
        }
        let mut segments = Vec::new();
        for s in text.split('/') {
            match s {
                "" | "." | ".." => {
                    return bad(format!(
                        "path pattern {text:?} has an empty, `.` or `..` segment"
                    ));
                }
                "**" => segments.push(Seg::Deep),
                s if s.contains("**") => {
                    return bad(format!("`**` must be a whole segment in {text:?}"));
                }
                s => segments.push(Seg::One(s.to_ascii_lowercase().into_bytes())),
            }
        }
        Ok(Self {
            text: text.to_owned(),
            segments,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether `path` (repository-relative, `/` separated) matches, ignoring ASCII case.
    pub fn matches(&self, path: &[u8]) -> bool {
        let lower: Vec<u8> = path.to_ascii_lowercase();
        let parts: Vec<&[u8]> = lower.split(|b| *b == b'/').collect();
        match_segments(&self.segments, &parts)
    }
}

fn match_segments(pat: &[Seg], parts: &[&[u8]]) -> bool {
    match pat.split_first() {
        None => parts.is_empty(),
        Some((Seg::Deep, rest)) => {
            // `**` swallows zero or more segments.
            (0..=parts.len()).any(|n| match_segments(rest, &parts[n..]))
        }
        Some((Seg::One(p), rest)) => match parts.split_first() {
            Some((first, tail)) => wild(p, first) && match_segments(rest, tail),
            None => false,
        },
    }
}

/// `*` any run, `?` one byte, within one segment.
fn wild(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ------------------------------------------------------------------------------ the policy

/// How a land onto a branch rewrites the pushed stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Rebase the stack onto the tip, merging file contents where the paths overlap.
    #[default]
    Rebase,
    /// Rebase, but a path both sides changed is a conflict; contents are never merged.
    RebaseDisjoint,
    /// No server rewrite: the pushed stack must already sit on the tip.
    FastForward,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::RebaseDisjoint => "rebase-disjoint",
            Self::FastForward => "fast-forward",
        }
    }
}

/// `[trunk]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trunk {
    pub require_approvals: u32,
    pub require_checks: Vec<String>,
    pub allow_force: bool,
}

/// `[paths."<glob>"]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRule {
    pub glob: PathGlob,
    pub write: Vec<Subject>,
}

/// `[agents.budgets]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budgets {
    pub max_changes_per_run: Option<u32>,
    pub max_paths_per_run: Option<u32>,
}

/// `[agents]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Agents {
    pub default_scope: Vec<PathGlob>,
    pub denied_paths: Vec<PathGlob>,
    pub budgets: Budgets,
}

/// A parsed, validated policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub roles: BTreeMap<String, Vec<Subject>>,
    pub trunk: Trunk,
    pub paths: Vec<PathRule>,
    pub agents: Agents,
    pub strategy: Strategy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    roles: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    trunk: RawTrunk,
    #[serde(default)]
    paths: BTreeMap<String, RawPathRule>,
    #[serde(default)]
    agents: RawAgents,
    #[serde(default)]
    merge: RawMerge,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTrunk {
    #[serde(default)]
    require_approvals: u32,
    #[serde(default)]
    require_checks: Vec<String>,
    #[serde(default)]
    allow_force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathRule {
    write: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAgents {
    #[serde(default)]
    default_scope: Vec<String>,
    #[serde(default)]
    denied_paths: Vec<String>,
    #[serde(default)]
    budgets: RawBudgets,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBudgets {
    max_changes_per_run: Option<u32>,
    max_paths_per_run: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMerge {
    #[serde(default)]
    strategy: Strategy,
}

fn subjects(list: &[String], allow_role: bool) -> Result<Vec<Subject>, PolicyError> {
    if list.len() > MAX_SUBJECTS {
        return bad("too many subjects in a list");
    }
    list.iter().map(|s| Subject::parse(s, allow_role)).collect()
}

fn globs(list: &[String]) -> Result<Vec<PathGlob>, PolicyError> {
    if list.len() > MAX_GLOBS {
        return bad("too many path patterns in a list");
    }
    list.iter().map(|g| PathGlob::parse(g)).collect()
}

fn budget(n: Option<u32>, what: &str) -> Result<Option<u32>, PolicyError> {
    match n {
        Some(0) => bad(format!("{what} must be at least 1; omit it for no limit")),
        Some(n) if n > MAX_BUDGET => bad(format!("{what} is above {MAX_BUDGET}")),
        n => Ok(n),
    }
}

impl Policy {
    /// Parses and validates a policy file. Strict: an unknown key, a wrong type, a bad subject or
    /// pattern, an undefined role, or no maintainer is an error, and nothing partial is returned.
    pub fn parse(bytes: &[u8]) -> Result<Self, PolicyError> {
        if bytes.len() > MAX_POLICY_BYTES {
            return bad(format!(
                "the policy file is larger than {MAX_POLICY_BYTES} bytes"
            ));
        }
        let text = std::str::from_utf8(bytes).or_else(|_| bad("the policy file is not UTF-8"))?;
        let raw: RawPolicy = toml::from_str(text)
            .map_err(|e| PolicyError(format!("policy.toml: {}", e.message())))?;
        if raw.version != POLICY_VERSION {
            return bad(format!(
                "policy version {} is not supported (this server reads {POLICY_VERSION})",
                raw.version
            ));
        }
        if raw.roles.len() > MAX_ROLES {
            return bad("too many roles");
        }
        let mut roles = BTreeMap::new();
        for (name, members) in &raw.roles {
            valid_role_name(name)?;
            roles.insert(name.clone(), subjects(members, false)?);
        }
        if roles.get(MAINTAINER).is_none_or(Vec::is_empty) {
            return bad("the policy must name at least one maintainer in [roles]");
        }
        if raw.trunk.require_approvals > MAX_APPROVALS {
            return bad(format!("require_approvals is above {MAX_APPROVALS}"));
        }
        if raw.trunk.require_checks.len() > MAX_CHECKS {
            return bad("too many require_checks");
        }
        for c in &raw.trunk.require_checks {
            if c.is_empty() || c.len() > MAX_NAME_LEN || c.chars().any(char::is_control) {
                return bad(
                    "a required check name is empty, too long or holds a control character",
                );
            }
        }
        if raw.paths.len() > MAX_PATH_RULES {
            return bad("too many [paths] rules");
        }
        let mut paths = Vec::with_capacity(raw.paths.len());
        for (glob, rule) in &raw.paths {
            let write = subjects(&rule.write, true)?;
            for s in &write {
                if let Subject::Role(r) = s
                    && !roles.contains_key(r)
                {
                    return bad(format!("path rule {glob:?} names the undefined role {r:?}"));
                }
            }
            paths.push(PathRule {
                glob: PathGlob::parse(glob)?,
                write,
            });
        }
        Ok(Self {
            roles,
            trunk: Trunk {
                require_approvals: raw.trunk.require_approvals,
                require_checks: raw.trunk.require_checks,
                allow_force: raw.trunk.allow_force,
            },
            paths,
            agents: Agents {
                default_scope: globs(&raw.agents.default_scope)?,
                denied_paths: globs(&raw.agents.denied_paths)?,
                budgets: Budgets {
                    max_changes_per_run: budget(
                        raw.agents.budgets.max_changes_per_run,
                        "max_changes_per_run",
                    )?,
                    max_paths_per_run: budget(
                        raw.agents.budgets.max_paths_per_run,
                        "max_paths_per_run",
                    )?,
                },
            },
            strategy: raw.merge.strategy,
        })
    }

    fn subject_matches(&self, who: &Who, s: &Subject) -> bool {
        match s {
            Subject::Anyone => true,
            Subject::Of { kind, glob } => who.kind == Some(*kind) && name_glob(glob, &who.name),
            Subject::Role(r) => self.is_member(who, r),
        }
    }

    /// Whether `who` is one of the subjects of role `role`.
    pub fn is_member(&self, who: &Who, role: &str) -> bool {
        self.roles
            .get(role)
            .is_some_and(|members| members.iter().any(|s| self.subject_matches(who, s)))
    }

    /// Whether `who` is a maintainer.
    pub fn is_maintainer(&self, who: &Who) -> bool {
        self.is_member(who, MAINTAINER)
    }

    /// Whether `who` is one of `list`.
    pub fn admits(&self, who: &Who, list: &[Subject]) -> bool {
        list.iter().any(|s| self.subject_matches(who, s))
    }

    /// Whether `who` may write `path` to a protected ref: the policy file needs a maintainer,
    /// an agent keeps to its scope and out of its denied paths, and every matching `[paths]` rule
    /// must admit the writer. A path no rule names is open to anyone who can write the ref.
    pub fn check_write(&self, who: &Who, path: &[u8]) -> Result<(), Denial> {
        let deny = |reason: String| Denial {
            path: String::from_utf8_lossy(path).into_owned(),
            reason,
        };
        if touches_policy(path) && !self.is_maintainer(who) {
            return Err(deny(
                "the repository policy can be changed only by a maintainer".into(),
            ));
        }
        let rules = || self.paths.iter().filter(|r| r.glob.matches(path));
        if who.is_agent() {
            if self.agents.denied_paths.iter().any(|g| g.matches(path)) {
                return Err(deny("agents may not write this path".into()));
            }
            if !self.agents.default_scope.is_empty() {
                let in_scope = self.agents.default_scope.iter().any(|g| g.matches(path));
                // A rule that names agents, and this one, extends the scope.
                let granted = rules().any(|r| {
                    r.write.iter().any(|s| {
                        matches!(
                            s,
                            Subject::Of {
                                kind: Kind::Agent,
                                ..
                            }
                        ) && self.subject_matches(who, s)
                    })
                });
                if !in_scope && !granted {
                    return Err(deny("this path is outside the agent scope".into()));
                }
            }
        }
        for r in rules() {
            if !self.admits(who, &r.write) {
                return Err(deny(format!(
                    "writing {} is limited to {}",
                    r.glob.as_str(),
                    if r.write.is_empty() {
                        "nobody".to_owned()
                    } else {
                        r.write
                            .iter()
                            .map(Subject::render)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            }
        }
        Ok(())
    }

    /// Whether an agent run of `changes` changesets touching `paths` paths is within budget.
    /// Anyone else has no budget.
    pub fn check_run(&self, who: &Who, changes: usize, paths: usize) -> Result<(), Denial> {
        if !who.is_agent() {
            return Ok(());
        }
        let over = |n: usize, max: Option<u32>| max.is_some_and(|m| n > m as usize);
        let b = &self.agents.budgets;
        if over(changes, b.max_changes_per_run) {
            return Err(Denial {
                path: String::new(),
                reason: format!(
                    "an agent run may land at most {} changes",
                    b.max_changes_per_run.unwrap_or_default()
                ),
            });
        }
        if over(paths, b.max_paths_per_run) {
            return Err(Denial {
                path: String::new(),
                reason: format!(
                    "an agent run may touch at most {} paths",
                    b.max_paths_per_run.unwrap_or_default()
                ),
            });
        }
        Ok(())
    }
}

/// Whether a change to `path` can alter [`POLICY_PATH`]: the file, a parent that is replaced by a
/// file, or something below it.
pub fn touches_policy(path: &[u8]) -> bool {
    let lower = path.to_ascii_lowercase();
    let policy = POLICY_PATH.as_bytes();
    lower == policy
        || lower == b".asc"
        || (lower.len() > policy.len() && lower.starts_with(policy) && lower[policy.len()] == b'/')
}

/// A write the policy refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// The path, empty for a whole-run refusal.
    pub path: String,
    pub reason: String,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.reason)
        } else {
            write!(f, "{}: {}", self.path, self.reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
version = 1
[roles]
maintainer = ["user:alice", "user:bob"]
reviewer = ["user:carol", "agent:review-*"]
[trunk]
require_approvals = 2
require_checks = ["build", "test"]
[paths."src/**"]
write = ["role:maintainer", "agent:*"]
[paths."src/secret/**"]
write = ["role:maintainer"]
[paths."docs/*.md"]
write = ["*"]
[agents]
default_scope = ["src/**", "tests/**"]
denied_paths = [".github/**"]
[agents.budgets]
max_changes_per_run = 3
max_paths_per_run = 10
[merge]
strategy = "rebase-disjoint"
"#;

    fn policy() -> Policy {
        Policy::parse(FULL.as_bytes()).unwrap()
    }

    fn user(n: &str) -> Who {
        Who::new(Kind::User, n)
    }

    fn agent(n: &str) -> Who {
        Who::new(Kind::Agent, n)
    }

    #[test]
    fn the_full_example_parses() {
        let p = policy();
        assert_eq!(p.trunk.require_approvals, 2);
        assert_eq!(p.trunk.require_checks, ["build", "test"]);
        assert!(!p.trunk.allow_force);
        assert_eq!(p.paths.len(), 3);
        assert_eq!(p.strategy, Strategy::RebaseDisjoint);
        assert_eq!(p.agents.budgets.max_changes_per_run, Some(3));
        let minimal = "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n";
        let m = Policy::parse(minimal.as_bytes()).unwrap();
        assert_eq!(m.strategy, Strategy::Rebase);
        assert_eq!(m.trunk, Trunk::default());
    }

    #[test]
    fn invalid_files_are_errors_never_partial_policies() {
        let head = "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n";
        let cases: Vec<(String, &str)> = vec![
            (String::new(), "empty file"),
            (
                "version = 2\n[roles]\nmaintainer = [\"user:a\"]\n".into(),
                "version",
            ),
            (
                "version = 1\n[roles]\nmaintainer = []\n".into(),
                "empty maintainers",
            ),
            ("version = 1\n".into(), "no roles"),
            (
                "version = 1\n[roles]\nreviewer = [\"user:a\"]\n".into(),
                "no maintainer",
            ),
            (format!("{head}[trunk]\nbogus = 1\n"), "unknown trunk key"),
            (format!("{head}bogus = 1\n"), "unknown top-level key"),
            (
                format!("{head}[agents]\nbogus = []\n"),
                "unknown agents key",
            ),
            (
                format!("{head}[agents.budgets]\nmax_changes_per_run = 0\n"),
                "zero budget",
            ),
            (
                format!("{head}[agents.budgets]\nmax_changes_per_run = 100001\n"),
                "huge budget",
            ),
            (
                format!("{head}[trunk]\nrequire_approvals = 17\n"),
                "too many approvals",
            ),
            (
                format!("{head}[trunk]\nrequire_approvals = -1\n"),
                "negative",
            ),
            (
                format!("{head}[merge]\nstrategy = \"squash\"\n"),
                "unknown strategy",
            ),
            (
                format!("{head}[paths.\"a\"]\nwrite = [\"role:ghost\"]\n"),
                "undefined role",
            ),
            (
                format!("{head}[paths.\"a\"]\nwrite = [\"nobody\"]\n"),
                "bad subject",
            ),
            (
                format!("{head}[paths.\"a\"]\nwrite = [\"robot:x\"]\n"),
                "unknown kind",
            ),
            (format!("{head}[paths.\"../a\"]\nwrite = []\n"), "dotdot"),
            (format!("{head}[paths.\"/a\"]\nwrite = []\n"), "absolute"),
            (
                format!("{head}[paths.\"a//b\"]\nwrite = []\n"),
                "empty segment",
            ),
            (
                format!("{head}[paths.\"a**\"]\nwrite = []\n"),
                "partial doublestar",
            ),
            (format!("{head}[paths.\"a\"]\n"), "missing write"),
            (
                format!("{head}[trunk]\nrequire_checks = [\"\"]\n"),
                "empty check",
            ),
            (
                "version = 1\n[roles]\nmaintainer = [\"role:maintainer\"]\n".into(),
                "role in role",
            ),
            (
                "version = 1\n[roles]\nmaintainer = [\"*\"]\n".into(),
                "everyone is a maintainer",
            ),
            (
                "version = 1\n[roles]\nMaint = [\"user:a\"]\n".into(),
                "role name",
            ),
        ];
        for (text, why) in cases {
            assert!(Policy::parse(text.as_bytes()).is_err(), "{why}");
        }
        assert!(Policy::parse(&[0xff, 0xfe]).is_err());
        let big = format!("{head}# {}\n", "x".repeat(MAX_POLICY_BYTES));
        assert!(Policy::parse(big.as_bytes()).is_err());
    }

    #[test]
    fn lists_are_bounded() {
        let head = "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n";
        let many: Vec<String> = (0..=MAX_SUBJECTS)
            .map(|i| format!("\"user:u{i}\""))
            .collect();
        let t = format!("version = 1\n[roles]\nmaintainer = [{}]\n", many.join(","));
        assert!(Policy::parse(t.as_bytes()).is_err());
        let checks: Vec<String> = (0..=MAX_CHECKS).map(|i| format!("\"c{i}\"")).collect();
        let t = format!("{head}[trunk]\nrequire_checks = [{}]\n", checks.join(","));
        assert!(Policy::parse(t.as_bytes()).is_err());
        let mut t = head.to_owned();
        for i in 0..=MAX_PATH_RULES {
            t.push_str(&format!("[paths.\"p{i}\"]\nwrite = []\n"));
        }
        assert!(Policy::parse(t.as_bytes()).is_err());
        let long = format!(
            "{head}[paths.\"{}\"]\nwrite = []\n",
            "a".repeat(MAX_GLOB_LEN + 1)
        );
        assert!(Policy::parse(long.as_bytes()).is_err());
    }

    #[test]
    fn path_globs_match_by_segment_and_ignore_case() {
        let g = |p: &str| PathGlob::parse(p).unwrap();
        assert!(g("src/**").matches(b"src/a"));
        assert!(g("src/**").matches(b"src/a/b/c.rs"));
        assert!(g("src/**").matches(b"SRC/A"));
        assert!(g("src/**").matches(b"src"), "`**` also matches no segment");
        assert!(!g("src/**").matches(b"srcx/a"));
        assert!(g("**/secret").matches(b"secret"));
        assert!(g("**/secret").matches(b"a/b/secret"));
        assert!(g("a/**/z").matches(b"a/z"));
        assert!(g("a/**/z").matches(b"a/b/c/z"));
        assert!(g("docs/*.md").matches(b"docs/x.md"));
        assert!(!g("docs/*.md").matches(b"docs/a/x.md"));
        assert!(g("a?c").matches(b"abc"));
        assert!(!g("a?c").matches(b"ac"));
        assert!(g("**").matches(b"anything/at/all"));
        assert!(!g("exact").matches(b"exact/child"));
    }

    #[test]
    fn subjects_match_by_kind_and_name_and_roles_expand() {
        let p = policy();
        assert!(p.is_maintainer(&user("alice")));
        assert!(!p.is_maintainer(&user("carol")));
        // A name is not enough: the kind must match.
        assert!(!p.is_maintainer(&agent("alice")));
        assert!(p.is_member(&agent("review-7"), "reviewer"));
        assert!(!p.is_member(&agent("other"), "reviewer"));
        assert!(!p.is_maintainer(&Who::unknown()));
        assert!(p.admits(&Who::unknown(), &[Subject::Anyone]));
        assert!(!p.admits(&Who::unknown(), &[]));
    }

    #[test]
    fn writes_follow_every_matching_rule() {
        let p = policy();
        let ok = |w: &Who, path: &str| p.check_write(w, path.as_bytes()).is_ok();
        // src/** admits maintainers and agents; src/secret/** only maintainers: both must admit.
        assert!(ok(&user("alice"), "src/lib.rs"));
        assert!(!ok(&user("carol"), "src/lib.rs"));
        assert!(ok(&agent("bot"), "src/lib.rs"));
        assert!(!ok(&agent("bot"), "src/secret/key"));
        assert!(!ok(&agent("bot"), "SRC/Secret/key"));
        assert!(ok(&user("alice"), "src/secret/key"));
        // Unnamed paths are open; `*` admits everyone, including unknown principals.
        assert!(ok(&user("carol"), "README.md"));
        assert!(ok(&Who::unknown(), "docs/a.md"));
        assert!(ok(&user("carol"), "docs/a.md"));
        let err = p.check_write(&user("carol"), b"src/lib.rs").unwrap_err();
        assert_eq!(err.path, "src/lib.rs");
        assert!(err.to_string().contains("role:maintainer"), "{err}");
    }

    #[test]
    fn agents_keep_to_their_scope_and_out_of_denied_paths() {
        let p = policy();
        let ok = |path: &str| p.check_write(&agent("bot"), path.as_bytes()).is_ok();
        assert!(ok("tests/a.rs"));
        assert!(ok("src/a.rs"));
        // Outside the scope and not named by a rule.
        assert!(!ok("README.md"));
        assert!(!ok("docs/a.md"), "`*` is not an explicit grant to agents");
        // Denied wins over everything.
        assert!(!ok(".github/workflows/ci.yml"));
        assert!(!ok(".GITHUB/x"));
        // A user is not bound by the agent lists.
        assert!(p.check_write(&user("carol"), b".github/x").is_ok());
        // An explicit agent rule extends the scope.
        let t = "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n[agents]\ndefault_scope = [\"src/**\"]\n[paths.\"docs/**\"]\nwrite = [\"agent:doc-*\"]\n";
        let p = Policy::parse(t.as_bytes()).unwrap();
        assert!(p.check_write(&agent("doc-bot"), b"docs/a").is_ok());
        assert!(p.check_write(&agent("other"), b"docs/a").is_err());
    }

    #[test]
    fn the_policy_file_needs_a_maintainer_whatever_the_rules_say() {
        let open = "version = 1\n[roles]\nmaintainer = [\"user:alice\"]\n[paths.\".asc/**\"]\nwrite = [\"*\"]\n";
        let p = Policy::parse(open.as_bytes()).unwrap();
        for path in [
            ".asc/policy.toml",
            ".ASC/Policy.toml",
            ".asc",
            ".asc/policy.toml/x",
        ] {
            assert!(
                p.check_write(&user("carol"), path.as_bytes()).is_err(),
                "{path}"
            );
            assert!(
                p.check_write(&user("alice"), path.as_bytes()).is_ok(),
                "{path}"
            );
        }
        // Other files under .asc follow the rules.
        assert!(p.check_write(&user("carol"), b".asc/review.toml").is_ok());
        assert!(!touches_policy(b".asc/policy.tomlx"));
        assert!(!touches_policy(b".ascx"));
    }

    #[test]
    fn runs_are_budgeted_for_agents_only() {
        let p = policy();
        assert!(p.check_run(&agent("bot"), 3, 10).is_ok());
        assert!(p.check_run(&agent("bot"), 4, 1).is_err());
        assert!(p.check_run(&agent("bot"), 1, 11).is_err());
        assert!(p.check_run(&user("carol"), 1000, 1000).is_ok());
        let none = Policy::parse(b"version = 1\n[roles]\nmaintainer = [\"user:a\"]\n").unwrap();
        assert!(none.check_run(&agent("bot"), 1_000_000, 1_000_000).is_ok());
    }
}
