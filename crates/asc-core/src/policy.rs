//! Repository policy as code: the schema, the strict parser and the pure decisions of
//! `.asc/policy.toml`.
//!
//! A repository may carry [`POLICY_PATH`] on its trunk. The file the server enforces is the one
//! on the trunk tip *before* an update, never the one in the update, so a change cannot lift the
//! rules that bind it. This module knows nothing about servers, stores or principals' wire forms:
//! it parses and validates the file, and answers "may this [`Who`] write this path", "does this
//! run fit the agent budget" and "what does landing on trunk need". The enforcement point is
//! `asc-serve`'s `policy` module.
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
//! [signing]
//! require_signed = true        # every change landing on trunk needs a VALID signature
//!
//! [paths."src/**"]             # every matching rule must admit the writer
//! write = ["role:maintainer", "agent:*"]
//!
//! [secrets]
//! enabled = true               # scan what an update adds (default true)
//! block_on_push = true         # refuse an update that adds a secret (default true)
//! allow_paths = ["tests/fixtures/**"]   # not scanned: a reviewed change, like any other
//! [secrets.rules.acme-key]     # a custom rule, reported as `custom.acme-key`
//! pattern = "ACME-[0-9A-F]{16}"
//! severity = "high"            # low | medium | high | critical (default high)
//! description = "ACME service key"
//!
//! [deps]
//! block_severity = "high"      # refuse a landing that adds a dependency with an advisory at or above this
//!                              # (low | moderate | high | critical); absent: the policy never blocks on one
//! block_unknown = true         # and refuse when the advisory lookup failed (default true; fail closed)
//!
//! [ci]
//! flaky_threshold = 0.02       # flips per run at which a test counts as flaky
//! allow_quarantine = false     # a maintainer may quarantine a flaky test
//!
//! [agents]
//! enabled = true               # agents may act in this repository
//! default_scope = ["src/**", "tests/**"]   # an agent writes only here, or where a rule names it
//! denied_paths = [".github/**"]            # an agent never writes here
//! max_runtime_seconds = 3600
//! [agents.budgets]
//! max_changes_per_run = 20     # a run is one ref-update request
//! max_paths_per_run = 200
//! max_tokens = 1000000         # model tokens one run may spend
//! max_cost_micro_usd = 5000000
//!
//! [merge]
//! strategy = "rebase"          # rebase | rebase-disjoint | fast-forward
//! queue = false                # land through the merge queue
//! revalidate = true            # the queue re-checks the exact head that would land
//! ```
//!
//! `[trunk] require_signed` is accepted as a synonym of `[signing] require_signed`; either one
//! turns the rule on.
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
/// Most `/`-separated segments in one path pattern.
pub const MAX_GLOB_SEGMENTS: usize = 32;
pub const MAX_SUBJECT_LEN: usize = 128;
pub const MAX_NAME_LEN: usize = 128;
/// Most approvals a policy can require, and the largest budget number.
pub const MAX_APPROVALS: u32 = 16;
pub const MAX_BUDGET: u32 = 100_000;
/// The role that may change the policy.
pub const MAINTAINER: &str = "maintainer";
/// Largest `max_runtime_seconds`: a week.
pub const MAX_RUNTIME_SECONDS: u32 = 7 * 24 * 3600;
/// Largest token budget of an agent run.
pub const MAX_TOKEN_BUDGET: u64 = 1_000_000_000;
/// Largest cost budget of an agent run, in millionths of a US dollar (one million dollars).
pub const MAX_COST_BUDGET: u64 = 1_000_000_000_000;
/// The flaky threshold when the policy names none.
pub const DEFAULT_FLAKY_THRESHOLD: f64 = 0.02;

/// Why a policy file was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PolicyError(pub String);

fn bad<T>(m: impl Into<String>) -> Result<T, PolicyError> {
    Err(PolicyError(m.into()))
}

/// A problem found in a policy file, located for an editor: `code` is a stable slug (`unknown_key`,
/// `bad_glob`, `no_maintainer`, `too_large`, ...), `line` and `column` are 1-based and 0 when the
/// problem is not tied to a place in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub line: u32,
    pub column: u32,
}

/// A parse failure with its classification and, for a syntax error, the byte span in the file.
struct Failure {
    code: &'static str,
    message: String,
    span: Option<std::ops::Range<usize>>,
}

fn fail<T>(code: &'static str, m: impl Into<String>) -> Result<T, Failure> {
    Err(Failure {
        code,
        message: m.into(),
        span: None,
    })
}

/// A validation refusal, classified by its message.
fn invalid<T>(m: impl Into<String>) -> Result<T, Failure> {
    Err(PolicyError(m.into()).into())
}

impl From<PolicyError> for Failure {
    fn from(e: PolicyError) -> Self {
        let code = classify(&e.0);
        Failure {
            code,
            message: e.0,
            span: None,
        }
    }
}

impl From<crate::secrets::RuleError> for Failure {
    fn from(e: crate::secrets::RuleError) -> Self {
        Failure {
            code: "bad_secret_rule",
            message: e.0,
            span: None,
        }
    }
}

/// The slug of a validation message. The messages are this module's own, so matching on a
/// fragment is stable; anything else is `invalid`.
fn classify(message: &str) -> &'static str {
    const TABLE: &[(&str, &str)] = &[
        ("too many", "too_many"),
        ("larger than", "too_large"),
        ("not UTF-8", "not_utf8"),
        ("not supported", "unsupported_version"),
        ("at least one maintainer", "no_maintainer"),
        ("undefined role", "undefined_role"),
        ("path pattern", "bad_glob"),
        ("`**`", "bad_glob"),
        ("subject", "bad_subject"),
        ("role name", "bad_role"),
        ("a role cannot", "bad_subject"),
        ("budget", "bad_budget"),
        ("max_", "bad_budget"),
        ("require_approvals", "bad_approvals"),
        ("required check", "bad_check"),
        ("secret rule", "bad_secret_rule"),
        ("flaky_threshold", "bad_threshold"),
    ];
    TABLE
        .iter()
        .find(|(frag, _)| message.contains(frag))
        .map_or("invalid", |(_, code)| code)
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
        if text.split('/').count() > MAX_GLOB_SEGMENTS {
            return bad(format!(
                "a path pattern has at most {MAX_GLOB_SEGMENTS} segments"
            ));
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

/// Whether `pat` matches all of `parts`. `**` swallows zero or more segments. The table
/// `next[j]` holds "the rest of the pattern matches `parts[j..]`", built from the last pattern
/// segment backwards, so the work is `segments x parts` and no `**` re-tries what another did.
fn match_segments(pat: &[Seg], parts: &[&[u8]]) -> bool {
    let n = parts.len();
    let mut next: Vec<bool> = (0..=n).map(|j| j == n).collect();
    for seg in pat.iter().rev() {
        let mut cur = vec![false; n + 1];
        match seg {
            Seg::Deep => {
                // Swallow nothing (`next[j]`) or one more segment (`cur[j + 1]`).
                for j in (0..=n).rev() {
                    cur[j] = next[j] || (j < n && cur[j + 1]);
                }
            }
            Seg::One(p) => {
                for j in 0..n {
                    cur[j] = next[j + 1] && wild(p, parts[j]);
                }
            }
        }
        next = cur;
    }
    next[0]
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

/// `[signing]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Signing {
    /// Every change landing on trunk needs a `VALID` signature.
    pub require_signed: bool,
}

/// `[paths."<glob>"]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRule {
    pub glob: PathGlob,
    pub write: Vec<Subject>,
}

/// `[secrets]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretsPolicy {
    /// Scan what an update adds. On unless the policy says otherwise.
    pub enabled: bool,
    /// Refuse an update that adds a secret; off, findings are recorded and the update goes on.
    pub block_on_push: bool,
    /// Paths that are not scanned. Read from the policy on the trunk tip, so changing the list is
    /// a reviewed change like any other.
    pub allow_paths: Vec<PathGlob>,
    /// `[secrets.rules.<id>]`, in id order.
    pub rules: Vec<crate::secrets::CustomRule>,
}

impl Default for SecretsPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            block_on_push: true,
            allow_paths: Vec::new(),
            rules: Vec::new(),
        }
    }
}

impl SecretsPolicy {
    /// Whether `path` is on the allow list.
    pub fn allows(&self, path: &[u8]) -> bool {
        self.allow_paths.iter().any(|g| g.matches(path))
    }
}

/// How serious an advisory about a dependency is, in the order the scale runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepSeverity {
    Low,
    Moderate,
    High,
    Critical,
}

impl DepSeverity {
    /// `low`, `moderate`, `high` or `critical` (`medium` is read as `moderate`), any case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "moderate" | "medium" => Some(Self::Moderate),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// `[deps]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepsPolicy {
    /// A landing that adds a dependency with an advisory at or above this is refused. `None`: the
    /// policy never blocks on advisories.
    pub block_severity: Option<DepSeverity>,
    /// With `block_severity` set, a landing is also refused when the advisory source could not be
    /// asked, because an unknown answer is not a clean one. On unless the policy says otherwise.
    pub block_unknown: bool,
}

impl Default for DepsPolicy {
    fn default() -> Self {
        Self {
            block_severity: None,
            block_unknown: true,
        }
    }
}

/// `[ci]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CiPolicy {
    /// Flips per run, above which a test counts as flaky.
    pub flaky_threshold: f64,
    /// A maintainer may quarantine a flaky test.
    pub allow_quarantine: bool,
}

impl Default for CiPolicy {
    fn default() -> Self {
        Self {
            flaky_threshold: DEFAULT_FLAKY_THRESHOLD,
            allow_quarantine: false,
        }
    }
}

/// `[agents.budgets]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budgets {
    pub max_changes_per_run: Option<u32>,
    pub max_paths_per_run: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_micro_usd: Option<u64>,
}

/// `[agents]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agents {
    /// Agents may act in this repository. On unless the policy says otherwise (the server
    /// switch is separate and also has to be on).
    pub enabled: bool,
    pub default_scope: Vec<PathGlob>,
    pub denied_paths: Vec<PathGlob>,
    pub max_runtime_seconds: Option<u32>,
    pub budgets: Budgets,
}

impl Default for Agents {
    fn default() -> Self {
        Self {
            enabled: true,
            default_scope: Vec::new(),
            denied_paths: Vec::new(),
            max_runtime_seconds: None,
            budgets: Budgets::default(),
        }
    }
}

/// `[merge]` besides the strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeOptions {
    /// Land through the merge queue.
    pub queue: bool,
    /// The queue re-checks the exact head that would land.
    pub revalidate: bool,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            queue: false,
            revalidate: true,
        }
    }
}

/// A parsed, validated policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub roles: BTreeMap<String, Vec<Subject>>,
    pub trunk: Trunk,
    pub signing: Signing,
    pub paths: Vec<PathRule>,
    pub secrets: SecretsPolicy,
    pub deps: DepsPolicy,
    pub ci: CiPolicy,
    pub agents: Agents,
    pub strategy: Strategy,
    pub merge: MergeOptions,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    roles: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    trunk: RawTrunk,
    #[serde(default)]
    signing: RawSigning,
    #[serde(default)]
    paths: BTreeMap<String, RawPathRule>,
    #[serde(default)]
    secrets: RawSecrets,
    #[serde(default)]
    deps: RawDeps,
    #[serde(default)]
    ci: RawCi,
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
    /// A synonym of `[signing] require_signed`.
    #[serde(default)]
    require_signed: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSigning {
    #[serde(default)]
    require_signed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathRule {
    write: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecrets {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default = "yes")]
    block_on_push: bool,
    #[serde(default)]
    allow_paths: Vec<String>,
    #[serde(default)]
    rules: BTreeMap<String, RawSecretRule>,
}

impl Default for RawSecrets {
    fn default() -> Self {
        Self {
            enabled: true,
            block_on_push: true,
            allow_paths: Vec::new(),
            rules: BTreeMap::new(),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecretRule {
    pattern: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeps {
    block_severity: Option<String>,
    #[serde(default = "yes")]
    block_unknown: bool,
}

impl Default for RawDeps {
    fn default() -> Self {
        Self {
            block_severity: None,
            block_unknown: true,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCi {
    flaky_threshold: Option<f64>,
    #[serde(default)]
    allow_quarantine: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgents {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    default_scope: Vec<String>,
    #[serde(default)]
    denied_paths: Vec<String>,
    max_runtime_seconds: Option<u32>,
    #[serde(default)]
    budgets: RawBudgets,
}

impl Default for RawAgents {
    fn default() -> Self {
        Self {
            enabled: true,
            default_scope: Vec::new(),
            denied_paths: Vec::new(),
            max_runtime_seconds: None,
            budgets: RawBudgets::default(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBudgets {
    max_changes_per_run: Option<u32>,
    max_paths_per_run: Option<u32>,
    max_tokens: Option<u64>,
    max_cost_micro_usd: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMerge {
    #[serde(default)]
    strategy: Strategy,
    #[serde(default)]
    queue: bool,
    #[serde(default = "yes")]
    revalidate: bool,
}

impl Default for RawMerge {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            queue: false,
            revalidate: true,
        }
    }
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

fn big_budget(n: Option<u64>, what: &str, max: u64) -> Result<Option<u64>, PolicyError> {
    match n {
        Some(0) => bad(format!("{what} must be at least 1; omit it for no limit")),
        Some(n) if n > max => bad(format!("{what} is above {max}")),
        n => Ok(n),
    }
}

/// What landing on trunk needs, as the policy states it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandRequirements {
    pub approvals: u32,
    pub checks: Vec<String>,
    pub signed: bool,
    pub strategy: Strategy,
}

impl Policy {
    /// Parses and validates a policy file. Strict: an unknown key, a wrong type, a bad subject or
    /// pattern, an undefined role, or no maintainer is an error, and nothing partial is returned.
    pub fn parse(bytes: &[u8]) -> Result<Self, PolicyError> {
        Self::parse_located(bytes).map_err(|f| PolicyError(f.message))
    }

    /// [`parse`](Self::parse), but a refusal is a [`Diagnostic`] with a code and, where the file is
    /// not valid TOML or holds an unknown key, the line and column.
    pub fn diagnose(bytes: &[u8]) -> Result<Self, Diagnostic> {
        Self::parse_located(bytes).map_err(|f| {
            let (line, column) = match (&f.span, std::str::from_utf8(bytes)) {
                (Some(span), Ok(text)) => locate(text, span.start),
                _ => (0, 0),
            };
            Diagnostic {
                code: f.code,
                message: f.message,
                line,
                column,
            }
        })
    }

    fn parse_located(bytes: &[u8]) -> Result<Self, Failure> {
        if bytes.len() > MAX_POLICY_BYTES {
            return fail(
                "too_large",
                format!("the policy file is larger than {MAX_POLICY_BYTES} bytes"),
            );
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return fail("not_utf8", "the policy file is not UTF-8");
        };
        let raw: RawPolicy = toml::from_str(text).map_err(|e| Failure {
            code: toml_code(e.message()),
            message: format!("policy.toml: {}", e.message()),
            span: e.span(),
        })?;
        if raw.version != POLICY_VERSION {
            return fail(
                "unsupported_version",
                format!(
                    "policy version {} is not supported (this server reads {POLICY_VERSION})",
                    raw.version
                ),
            );
        }
        Self::build(raw)
    }

    fn build(raw: RawPolicy) -> Result<Self, Failure> {
        if raw.roles.len() > MAX_ROLES {
            return fail("too_many", "too many roles");
        }
        let mut roles = BTreeMap::new();
        for (name, members) in &raw.roles {
            valid_role_name(name)?;
            roles.insert(name.clone(), subjects(members, false)?);
        }
        if roles.get(MAINTAINER).is_none_or(Vec::is_empty) {
            return invalid("the policy must name at least one maintainer in [roles]");
        }
        if raw.trunk.require_approvals > MAX_APPROVALS {
            return invalid(format!("require_approvals is above {MAX_APPROVALS}"));
        }
        if raw.trunk.require_checks.len() > MAX_CHECKS {
            return invalid("too many require_checks");
        }
        for c in &raw.trunk.require_checks {
            if c.is_empty() || c.len() > MAX_NAME_LEN || c.chars().any(char::is_control) {
                return invalid(
                    "a required check name is empty, too long or holds a control character",
                );
            }
        }
        if raw.paths.len() > MAX_PATH_RULES {
            return invalid("too many [paths] rules");
        }
        let mut paths = Vec::with_capacity(raw.paths.len());
        for (glob, rule) in &raw.paths {
            let write = subjects(&rule.write, true)?;
            for s in &write {
                if let Subject::Role(r) = s
                    && !roles.contains_key(r)
                {
                    return invalid(format!("path rule {glob:?} names the undefined role {r:?}"));
                }
            }
            paths.push(PathRule {
                glob: PathGlob::parse(glob)?,
                write,
            });
        }
        let secrets = build_secrets(raw.secrets)?;
        let deps = build_deps(raw.deps)?;
        let ci = build_ci(raw.ci)?;
        if raw.agents.max_runtime_seconds == Some(0)
            || raw
                .agents
                .max_runtime_seconds
                .is_some_and(|s| s > MAX_RUNTIME_SECONDS)
        {
            return invalid(format!(
                "max_runtime_seconds must be between 1 and {MAX_RUNTIME_SECONDS}"
            ));
        }
        let b = &raw.agents.budgets;
        let budgets = Budgets {
            max_changes_per_run: budget(b.max_changes_per_run, "max_changes_per_run")?,
            max_paths_per_run: budget(b.max_paths_per_run, "max_paths_per_run")?,
            max_tokens: big_budget(b.max_tokens, "max_tokens", MAX_TOKEN_BUDGET)?,
            max_cost_micro_usd: big_budget(
                b.max_cost_micro_usd,
                "max_cost_micro_usd",
                MAX_COST_BUDGET,
            )?,
        };
        Ok(Self {
            roles,
            trunk: Trunk {
                require_approvals: raw.trunk.require_approvals,
                require_checks: raw.trunk.require_checks,
                allow_force: raw.trunk.allow_force,
            },
            signing: Signing {
                require_signed: raw.signing.require_signed || raw.trunk.require_signed,
            },
            paths,
            secrets,
            deps,
            ci,
            agents: Agents {
                enabled: raw.agents.enabled,
                default_scope: globs(&raw.agents.default_scope)?,
                denied_paths: globs(&raw.agents.denied_paths)?,
                max_runtime_seconds: raw.agents.max_runtime_seconds,
                budgets,
            },
            strategy: raw.merge.strategy,
            merge: MergeOptions {
                queue: raw.merge.queue,
                revalidate: raw.merge.revalidate,
            },
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
            if !self.agents.enabled {
                return Err(deny("agents are switched off for this repository".into()));
            }
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
        if !self.agents.enabled {
            return Err(Denial {
                path: String::new(),
                reason: "agents are switched off for this repository".into(),
            });
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

    /// What landing on trunk needs: approvals, checks, a signature and the merge strategy.
    pub fn land_requirements(&self) -> LandRequirements {
        LandRequirements {
            approvals: self.trunk.require_approvals,
            checks: self.trunk.require_checks.clone(),
            signed: self.signing.require_signed,
            strategy: self.strategy,
        }
    }

    /// Whether `who` may rewrite or delete trunk. A maintainer never may unless `allow_force`:
    /// the switch is the policy's, and changing it is a reviewed change.
    pub fn check_force(&self) -> Result<(), Denial> {
        if self.trunk.allow_force {
            Ok(())
        } else {
            Err(Denial {
                path: String::new(),
                reason: "trunk cannot be rewritten or deleted (allow_force is off)".into(),
            })
        }
    }

    /// The members of role `maintainer` that are agents or the open `*`: a warning for an editor,
    /// since an agent that can change the policy can lift every rule that binds it.
    pub fn maintainers_include_agents(&self) -> bool {
        self.roles.get(MAINTAINER).is_some_and(|m| {
            m.iter().any(|s| {
                matches!(
                    s,
                    Subject::Of {
                        kind: Kind::Agent,
                        ..
                    } | Subject::Anyone
                )
            })
        })
    }
}

/// `[secrets]` of a raw policy: the allow list and the custom rules, validated.
fn build_secrets(raw: RawSecrets) -> Result<SecretsPolicy, Failure> {
    if raw.rules.len() > crate::secrets::MAX_CUSTOM_RULES {
        return invalid("too many secret rules");
    }
    let mut rules = Vec::with_capacity(raw.rules.len());
    for (id, r) in raw.rules {
        let severity = match r.severity.as_deref() {
            None => crate::secrets::Severity::High,
            Some(s) => crate::secrets::Severity::parse(s).ok_or_else(|| {
                PolicyError(format!(
                    "secret rule {id:?}: severity {s:?} is not low, medium, high or critical"
                ))
            })?,
        };
        let rule = crate::secrets::CustomRule {
            id,
            pattern: r.pattern,
            severity,
            description: r.description,
        };
        rule.validate()?;
        rules.push(rule);
    }
    Ok(SecretsPolicy {
        enabled: raw.enabled,
        block_on_push: raw.block_on_push,
        allow_paths: globs(&raw.allow_paths)?,
        rules,
    })
}

fn build_deps(raw: RawDeps) -> Result<DepsPolicy, Failure> {
    let block_severity = match raw.block_severity.as_deref() {
        None => None,
        Some(s) => Some(DepSeverity::parse(s).ok_or_else(|| {
            PolicyError(format!(
                "[deps] block_severity {s:?} is not low, moderate, high or critical"
            ))
        })?),
    };
    Ok(DepsPolicy {
        block_severity,
        block_unknown: raw.block_unknown,
    })
}

fn build_ci(raw: RawCi) -> Result<CiPolicy, Failure> {
    let flaky_threshold = match raw.flaky_threshold {
        None => DEFAULT_FLAKY_THRESHOLD,
        Some(t) if t.is_finite() && t > 0.0 && t <= 1.0 => t,
        Some(_) => return invalid("flaky_threshold must be above 0 and at most 1"),
    };
    Ok(CiPolicy {
        flaky_threshold,
        allow_quarantine: raw.allow_quarantine,
    })
}

/// The 1-based line and byte column of `offset` in `text`.
fn locate(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let before = &text.as_bytes()[..offset];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(offset - start + 1).unwrap_or(u32::MAX),
    )
}

/// The slug of a TOML or schema error from the deserializer.
fn toml_code(message: &str) -> &'static str {
    if message.contains("unknown field") {
        "unknown_key"
    } else if message.contains("missing field") {
        "missing_key"
    } else if message.contains("invalid type") || message.contains("invalid value") {
        "bad_type"
    } else {
        "syntax"
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
    fn many_double_stars_match_in_time_proportional_to_the_input() {
        let g = |p: &str| PathGlob::parse(p).unwrap();
        // 30 `**` segments and a path that cannot match used to take exponential time.
        let pat = format!("{}z", "**/".repeat(30));
        let deep = vec!["a"; 200].join("/");
        let started = std::time::Instant::now();
        assert!(!g(&pat).matches(deep.as_bytes()));
        assert!(g(&pat).matches(format!("{deep}/z").as_bytes()));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // Interleaved with wildcards, still exact.
        assert!(g("**/a/**/b/**/c").matches(b"x/a/y/b/z/c"));
        assert!(!g("**/a/**/b/**/c").matches(b"x/a/y/c/z/b"));
        assert!(g("**/**/x").matches(b"x"));
    }

    #[test]
    fn a_path_pattern_has_a_bounded_number_of_segments() {
        let ok = vec!["a"; MAX_GLOB_SEGMENTS].join("/");
        assert!(PathGlob::parse(&ok).is_ok());
        let too_many = vec!["a"; MAX_GLOB_SEGMENTS + 1].join("/");
        assert!(PathGlob::parse(&too_many).is_err());
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

    const HEAD: &str = "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n";

    const EVERYTHING: &str = r#"
version = 1
[roles]
maintainer = ["user:alice"]
[trunk]
require_approvals = 1
[signing]
require_signed = true
[secrets]
enabled = true
block_on_push = false
allow_paths = ["tests/fixtures/**", "docs/*.md"]
[secrets.rules.acme-key]
pattern = "ACME-[0-9A-F]{16}"
severity = "critical"
description = "ACME service key"
[secrets.rules.plain]
pattern = "internal-[a-z]{8}"
[ci]
flaky_threshold = 0.1
allow_quarantine = true
[agents]
enabled = false
max_runtime_seconds = 600
[agents.budgets]
max_changes_per_run = 5
max_tokens = 100000
max_cost_micro_usd = 2500000
[merge]
queue = true
revalidate = false
"#;

    #[test]
    fn every_key_of_the_contract_parses_and_defaults_are_safe() {
        let p = Policy::parse(EVERYTHING.as_bytes()).unwrap();
        assert!(p.signing.require_signed);
        assert!(p.secrets.enabled && !p.secrets.block_on_push);
        assert_eq!(p.secrets.allow_paths.len(), 2);
        assert!(p.secrets.allows(b"tests/fixtures/a/b.pem"));
        assert!(p.secrets.allows(b"DOCS/readme.md"));
        assert!(!p.secrets.allows(b"src/a.rs"));
        assert_eq!(p.secrets.rules.len(), 2);
        assert_eq!(p.secrets.rules[0].id, "acme-key");
        assert_eq!(
            p.secrets.rules[0].severity,
            crate::secrets::Severity::Critical
        );
        assert_eq!(
            p.secrets.rules[1].severity,
            crate::secrets::Severity::High,
            "default"
        );
        assert_eq!(p.ci.flaky_threshold, 0.1);
        assert!(p.ci.allow_quarantine);
        assert!(!p.agents.enabled);
        assert_eq!(p.agents.max_runtime_seconds, Some(600));
        assert_eq!(p.agents.budgets.max_tokens, Some(100_000));
        assert_eq!(p.agents.budgets.max_cost_micro_usd, Some(2_500_000));
        assert!(p.merge.queue && !p.merge.revalidate);

        // A bare file is the safe default: scanning on and blocking, nothing signed, agents allowed
        // by the file, the queue off and revalidating.
        let d = Policy::parse(HEAD.as_bytes()).unwrap();
        assert_eq!(d.secrets, SecretsPolicy::default());
        assert!(d.secrets.enabled && d.secrets.block_on_push);
        assert!(!d.signing.require_signed);
        assert_eq!(d.ci, CiPolicy::default());
        assert_eq!(d.ci.flaky_threshold, 0.02);
        assert!(d.agents.enabled);
        assert_eq!(
            d.merge,
            MergeOptions {
                queue: false,
                revalidate: true
            }
        );
    }

    #[test]
    fn the_deps_section_blocks_only_when_it_names_a_severity() {
        let d = Policy::parse(HEAD.as_bytes()).unwrap().deps;
        assert_eq!(d, DepsPolicy::default());
        assert_eq!(d.block_severity, None);
        assert!(d.block_unknown, "an unknown answer is not a clean one");
        let p = |extra: &str| Policy::parse(format!("{HEAD}{extra}").as_bytes());
        let on = p("[deps]
block_severity = \"High\"
block_unknown = false
")
            .unwrap()
            .deps;
        assert_eq!(on.block_severity, Some(DepSeverity::High));
        assert!(!on.block_unknown);
        let medium = p("[deps]
block_severity = \"medium\"
").unwrap().deps;
        assert_eq!(medium.block_severity, Some(DepSeverity::Moderate));
        assert!(DepSeverity::Low < DepSeverity::Moderate && DepSeverity::High < DepSeverity::Critical);
        assert_eq!(DepSeverity::parse("critical").unwrap().name(), "critical");
        assert!(p("[deps]
block_severity = \"severe\"
").is_err());
        assert!(p("[deps]
block = \"high\"
").is_err(), "unknown keys are errors");
    }

    #[test]
    fn require_signed_has_two_spellings_and_either_turns_it_on() {
        let signed = |extra: &str| {
            Policy::parse(format!("{HEAD}{extra}").as_bytes())
                .unwrap()
                .signing
                .require_signed
        };
        assert!(signed("[trunk]\nrequire_signed = true\n"));
        assert!(signed("[signing]\nrequire_signed = true\n"));
        assert!(
            signed("[signing]\nrequire_signed = false\n[trunk]\nrequire_signed = true\n"),
            "either wins"
        );
        assert!(!signed("[signing]\nrequire_signed = false\n"));
        assert!(!signed(""));
        assert!(Policy::parse(format!("{HEAD}[signing]\nbogus = 1\n").as_bytes()).is_err());
    }

    #[test]
    fn the_new_sections_are_strict_and_bounded() {
        let bad = |extra: &str| Policy::parse(format!("{HEAD}{extra}").as_bytes()).is_err();
        assert!(bad("[secrets]\nbogus = true\n"), "unknown secrets key");
        assert!(bad("[secrets]\nenabled = \"yes\"\n"), "wrong type");
        assert!(
            bad("[secrets]\nallow_paths = [\"/abs\"]\n"),
            "bad allow path"
        );
        assert!(
            bad("[secrets]\nallow_paths = [\"../up\"]\n"),
            "dotdot allow path"
        );
        assert!(bad("[secrets.rules.x]\n"), "pattern is required");
        assert!(
            bad("[secrets.rules.x]\npattern = \"(\"\n"),
            "pattern must compile"
        );
        assert!(bad("[secrets.rules.x]\npattern = \"a*\"\n"), "empty match");
        assert!(
            bad("[secrets.rules.x]\npattern = \"a\"\nseverity = \"urgent\"\n"),
            "severity"
        );
        assert!(
            bad("[secrets.rules.x]\npattern = \"a\"\nbogus = 1\n"),
            "unknown rule key"
        );
        assert!(bad("[secrets.rules.Bad]\npattern = \"a\"\n"), "rule id");
        assert!(bad("[ci]\nflaky_threshold = 0\n"));
        assert!(bad("[ci]\nflaky_threshold = 1.5\n"));
        assert!(bad("[ci]\nflaky_threshold = -0.1\n"));
        assert!(bad("[ci]\nbogus = 1\n"));
        assert!(bad("[agents]\nmax_runtime_seconds = 0\n"));
        assert!(bad(&format!(
            "[agents]\nmax_runtime_seconds = {}\n",
            MAX_RUNTIME_SECONDS + 1
        )));
        assert!(bad("[agents.budgets]\nmax_tokens = 0\n"));
        assert!(bad(&format!(
            "[agents.budgets]\nmax_tokens = {}\n",
            MAX_TOKEN_BUDGET + 1
        )));
        assert!(bad("[agents.budgets]\nmax_cost_micro_usd = 0\n"));
        assert!(bad(&format!(
            "[agents.budgets]\nmax_cost_micro_usd = {}\n",
            MAX_COST_BUDGET + 1
        )));
        assert!(bad("[agents.budgets]\nmax_tokens = -5\n"));
        assert!(bad("[merge]\nqueue = 3\n"));
        assert!(bad("[merge]\nbogus = true\n"));
        let rules: String = (0..=crate::secrets::MAX_CUSTOM_RULES)
            .map(|i| format!("[secrets.rules.r{i}]\npattern = \"x{i}\"\n"))
            .collect();
        assert!(bad(&rules), "too many rules");
        let allow: Vec<String> = (0..=MAX_GLOBS).map(|i| format!("\"p{i}\"")).collect();
        assert!(bad(&format!(
            "[secrets]\nallow_paths = [{}]\n",
            allow.join(",")
        )));
        // The edges are fine.
        let ok = format!(
            "{HEAD}[ci]\nflaky_threshold = 1\n[agents]\nmax_runtime_seconds = {MAX_RUNTIME_SECONDS}\n[agents.budgets]\nmax_tokens = {MAX_TOKEN_BUDGET}\nmax_cost_micro_usd = {MAX_COST_BUDGET}\n"
        );
        Policy::parse(ok.as_bytes()).unwrap();
    }

    #[test]
    fn diagnostics_say_what_and_where() {
        let d = |text: &str| Policy::diagnose(text.as_bytes()).unwrap_err();
        let unknown = d(&format!("{HEAD}[trunk]\nbogus = 1\n"));
        assert_eq!(unknown.code, "unknown_key");
        assert_eq!(unknown.line, 5, "{unknown:?}");
        assert!(unknown.column >= 1);
        let syntax = d("version = 1\n[roles\n");
        assert_eq!(syntax.code, "syntax");
        assert_eq!(syntax.line, 2);
        assert_eq!(d("").code, "missing_key");
        assert_eq!(
            d("version = 2\n[roles]\nmaintainer=[\"user:a\"]\n").code,
            "unsupported_version"
        );
        assert_eq!(
            d("version = 1\n[roles]\nreviewer=[\"user:a\"]\n").code,
            "no_maintainer"
        );
        assert_eq!(
            d(&format!("{HEAD}[paths.\"/a\"]\nwrite=[]\n")).code,
            "bad_glob"
        );
        assert_eq!(
            d(&format!("{HEAD}[paths.\"a\"]\nwrite=[\"role:ghost\"]\n")).code,
            "undefined_role"
        );
        assert_eq!(
            d(&format!("{HEAD}[paths.\"a\"]\nwrite=[\"nobody\"]\n")).code,
            "bad_subject"
        );
        assert_eq!(
            d(&format!("{HEAD}[trunk]\nrequire_approvals = 99\n")).code,
            "bad_approvals"
        );
        assert_eq!(
            d(&format!(
                "{HEAD}[agents.budgets]\nmax_changes_per_run = 0\n"
            ))
            .code,
            "bad_budget"
        );
        assert_eq!(
            d(&format!("{HEAD}[secrets.rules.x]\npattern = \"(\"\n")).code,
            "bad_secret_rule"
        );
        assert_eq!(
            d(&format!("{HEAD}[ci]\nflaky_threshold = 2.0\n")).code,
            "bad_threshold"
        );
        assert_eq!(
            d(&format!("{HEAD}[trunk]\nrequire_approvals = \"x\"\n")).code,
            "bad_type"
        );
        let big = format!("{HEAD}# {}\n", "x".repeat(MAX_POLICY_BYTES));
        let too_large = Policy::diagnose(big.as_bytes()).unwrap_err();
        assert_eq!((too_large.code, too_large.line), ("too_large", 0));
        assert_eq!(Policy::diagnose(&[0xff]).unwrap_err().code, "not_utf8");
        assert!(Policy::diagnose(HEAD.as_bytes()).is_ok());
        // `parse` and `diagnose` agree on what is valid.
        for t in [EVERYTHING, HEAD, "x", ""] {
            assert_eq!(
                Policy::parse(t.as_bytes()).is_ok(),
                Policy::diagnose(t.as_bytes()).is_ok()
            );
        }
    }

    #[test]
    fn landing_requirements_and_force_follow_the_file() {
        let p = Policy::parse(EVERYTHING.as_bytes()).unwrap();
        let r = p.land_requirements();
        assert_eq!(r.approvals, 1);
        assert!(r.signed);
        assert_eq!(r.strategy, Strategy::Rebase);
        assert!(p.check_force().is_err());
        let open = format!("{HEAD}[trunk]\nallow_force = true\nrequire_checks = [\"a\", \"b\"]\n");
        let p = Policy::parse(open.as_bytes()).unwrap();
        assert!(p.check_force().is_ok());
        assert_eq!(p.land_requirements().checks, ["a", "b"]);
        assert!(!p.land_requirements().signed);
    }

    #[test]
    fn switching_agents_off_refuses_them_everywhere() {
        let p = Policy::parse(EVERYTHING.as_bytes()).unwrap();
        let e = p.check_write(&agent("bot"), b"src/a").unwrap_err();
        assert!(e.reason.contains("switched off"), "{e}");
        assert!(p.check_run(&agent("bot"), 1, 1).is_err());
        assert!(p.check_write(&user("carol"), b"src/a").is_ok());
        assert!(p.check_run(&user("carol"), 1, 1).is_ok());
    }

    #[test]
    fn a_maintainer_role_naming_agents_is_flagged() {
        let t = "version = 1\n[roles]\nmaintainer = [\"user:a\", \"agent:ci-*\"]\n";
        assert!(
            Policy::parse(t.as_bytes())
                .unwrap()
                .maintainers_include_agents()
        );
        assert!(!policy().maintainers_include_agents());
    }

    #[test]
    fn a_policy_answers_only_for_the_bytes_it_was_built_from() {
        // The policy that decides is the one parsed from the tip before the update; the file a
        // change adds matters only once it is the tip. A non-maintainer cannot write the file.
        let before = Policy::parse(FULL.as_bytes()).unwrap();
        let after_text = FULL.replace("require_approvals = 2", "require_approvals = 0");
        let after = Policy::parse(after_text.as_bytes()).unwrap();
        assert_eq!(before.trunk.require_approvals, 2);
        assert_eq!(after.trunk.require_approvals, 0);
        assert!(
            before
                .check_write(&user("carol"), POLICY_PATH.as_bytes())
                .is_err()
        );
        assert!(
            before
                .check_write(&agent("bot"), POLICY_PATH.as_bytes())
                .is_err()
        );
    }
}
