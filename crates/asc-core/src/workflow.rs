//! Repository workflows as code: the schema, the strict parser and the pure decisions of
//! `.asc/workflows/<name>.toml`.
//!
//! A workflow is agent work that starts on an event and runs steps in order. The file lives on
//! the trunk, so a change to it is reviewed like code, and the server runs the version on the
//! trunk tip at trigger time, never the one inside an update. This module knows nothing about
//! servers or stores: it parses and validates a file and answers "does this trigger match this
//! event" and "what does this template render to".
//!
//! ```toml
//! description = "Fix a failing check and prove the fix"
//! max_parallel_runs = 1
//!
//! [[trigger]]
//! kind = "check_failed"          # manual | change_opened | change_updated | check_failed
//! branches = ["main"]            # | comment_mention | land | schedule
//! paths = ["src/**"]
//!
//! [[trigger]]
//! kind = "schedule"
//! cron = "0 3 * * 1-5"           # UTC, five fields, never more often than the server allows
//!
//! [[input]]
//! name = "ticket"
//! required = false
//! default = ""
//!
//! [budget]                       # caps the sum over all steps
//! max_changes = 5
//! max_tokens = 400000
//! max_cost_micro_usd = 2000000
//! max_runtime_seconds = 3600
//!
//! [[step]]
//! id = "fix"
//! kind = "agent"
//! agent = "fixer"
//! prompt = "The check {{event.check}} failed on change {{change}}. Fix it. Ticket: {{input.ticket}}"
//! paths = ["src/**"]
//! timeout_seconds = 1800
//!
//! [[step]]
//! id = "test"
//! kind = "command"
//! needs = ["fix"]
//! command = ["cargo", "test", "-p", "core"]   # an argument list, never a shell line
//! secrets = { REGISTRY_TOKEN = "registry_token" }
//!
//! [[step]]
//! id = "ok"
//! kind = "approval"
//! needs = ["test"]
//! approvers = ["role:maintainer"]
//! ```
//!
//! # What a step is
//!
//! * `agent` runs an agent task (a prompt) with its own token, scope and budget. Templates
//!   `{{input.x}}`, `{{change}}` and `{{event.field}}` insert values into the prompt as text.
//! * `command` runs a fixed argument list in the sandbox over a working copy of the change.
//!   Templates are not expanded in `command`: event and input values reach the process only as
//!   the environment variables `ASC_INPUT_<NAME>`, `ASC_CHANGE` and `ASC_EVENT_<FIELD>`, so
//!   untrusted text can never become an argument. `secrets` maps environment variables to
//!   secrets of the repository.
//! * `approval` waits for one of `approvers` (policy subjects `user:<glob>` or `role:<name>`).
//! * `checks` waits for the named checks on the newest change of the run.
//!
//! # Bounds
//!
//! The file is at most [`MAX_WORKFLOW_BYTES`]; unknown keys are an error at every level; there
//! are at most [`MAX_STEPS`] steps and [`MAX_TRIGGERS`] triggers; a prompt is at most
//! [`MAX_PROMPT_BYTES`]; the `needs` graph is acyclic. An invalid file is an error, never a
//! partial workflow.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::policy::PathGlob;

/// The directory of workflow files on the trunk.
pub const WORKFLOW_DIR: &str = ".asc/workflows";
/// Largest workflow file.
pub const MAX_WORKFLOW_BYTES: usize = 64 * 1024;
/// Most steps in one workflow.
pub const MAX_STEPS: usize = 32;
/// Most triggers in one workflow.
pub const MAX_TRIGGERS: usize = 16;
/// Most inputs in one workflow.
pub const MAX_INPUTS: usize = 16;
/// Longest prompt, in bytes.
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;
/// Longest input value, in bytes.
pub const MAX_INPUT_VALUE_BYTES: usize = 4 * 1024;
/// Most globs in one list, most approvers, checks, needs and secrets per step.
pub const MAX_LIST: usize = 32;
/// Most arguments in a command, and the longest one.
pub const MAX_COMMAND_ARGS: usize = 64;
pub const MAX_COMMAND_ARG_BYTES: usize = 4096;
/// Longest name of a workflow, step, input, secret or environment variable.
pub const MAX_NAME_BYTES: usize = 64;
/// The step timeout that applies when a step names none.
pub const DEFAULT_AGENT_TIMEOUT_SECS: u32 = 3600;
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u32 = 900;
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u32 = 7 * 86_400;
pub const DEFAULT_CHECKS_TIMEOUT_SECS: u32 = 3600;
/// The longest timeout a step may ask for.
pub const MAX_TIMEOUT_SECS: u32 = 30 * 86_400;
/// Largest budget number a file may state.
pub const MAX_BUDGET_CHANGES: u32 = 100_000;

/// The event fields a template may name as `{{event.<field>}}`.
pub const EVENT_FIELDS: [&str; 11] = [
    "actor", "branch", "change", "check", "comment", "new", "old", "paths", "ref", "title",
    "version",
];

/// Why a workflow file was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct WorkflowError {
    pub message: String,
    /// 1-based line, 0 when the error is not about one place.
    pub line: u32,
    pub column: u32,
}

fn bad<T>(m: impl Into<String>) -> Result<T, WorkflowError> {
    Err(WorkflowError {
        message: m.into(),
        line: 0,
        column: 0,
    })
}

// ------------------------------------------------------------------------------ names

/// Whether `s` is a workflow, step, input or secret name: 1 to 64 bytes of `[a-z0-9_-]`, not
/// starting with `-` or `_`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_BYTES
        && !s.starts_with(['-', '_'])
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Whether `s` is an agent name: 1 to 40 bytes of `[a-z0-9-]`, not starting or ending with `-`.
pub fn valid_agent_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `s` is an environment variable name: `[A-Z][A-Z0-9_]*`, at most 64 bytes, and not one
/// the runner owns (`ASC_*`, `PATH` and the like).
pub fn valid_env_name(s: &str) -> bool {
    let Some(first) = s.bytes().next() else {
        return false;
    };
    s.len() <= MAX_NAME_BYTES
        && first.is_ascii_uppercase()
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !s.starts_with("ASC_")
        && !RESERVED_ENV.contains(&s)
}

/// Variables the runner sets itself; a secret cannot replace them.
pub const RESERVED_ENV: [&str; 12] = [
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "PYTHONPATH",
    "NODE_OPTIONS",
];

// ------------------------------------------------------------------------------ cron

/// A five-field cron expression (minute hour day-of-month month day-of-week), UTC.
///
/// Fields take `*`, a number, a range `a-b`, a list `a,b-c` and a step `*/n` or `a-b/n`. Month
/// and weekday take numbers only (weekday 0 or 7 is Sunday). As in POSIX cron, when both
/// day-of-month and day-of-week are restricted a day matches if either does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    text: String,
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    day_restricted: bool,
    weekday_restricted: bool,
}

fn field(
    text: &str,
    min: u32,
    max: u32,
    name: &str,
    fold7: bool,
) -> Result<(u64, bool), WorkflowError> {
    let mut bits = 0u64;
    // As in POSIX cron, a field that starts with `*` does not restrict the day.
    let restricted = !text.starts_with('*');
    if text.is_empty() || text.len() > 64 {
        return bad(format!("cron {name} field is empty or too long"));
    }
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                    err(format!("cron {name} step {s:?} is not a positive number"))
                })?;
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (num(a, min, max, name)?, num(b, min, max, name)?)
        } else {
            let n = num(range, min, max, name)?;
            (n, if part.contains('/') { max } else { n })
        };
        if lo > hi {
            return bad(format!("cron {name} range {range:?} runs backwards"));
        }
        let mut v = lo;
        while v <= hi {
            let bit = if fold7 && v == 7 { 0 } else { v };
            bits |= 1u64 << bit;
            v += step;
        }
    }
    Ok((bits, restricted))
}

fn err(m: String) -> WorkflowError {
    WorkflowError {
        message: m,
        line: 0,
        column: 0,
    }
}

fn num(s: &str, min: u32, max: u32, name: &str) -> Result<u32, WorkflowError> {
    s.parse::<u32>()
        .ok()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| err(format!("cron {name} value {s:?} is not in {min}..={max}")))
}

impl Cron {
    /// Parses an expression. Macros such as `@daily` are not accepted: a schedule is explicit.
    pub fn parse(text: &str) -> Result<Self, WorkflowError> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() != 5 {
            return bad(
                "a cron expression has five fields: minute hour day-of-month month day-of-week",
            );
        }
        let (minutes, _) = field(parts[0], 0, 59, "minute", false)?;
        let (hours, _) = field(parts[1], 0, 23, "hour", false)?;
        let (days, day_restricted) = field(parts[2], 1, 31, "day-of-month", false)?;
        let (months, _) = field(parts[3], 1, 12, "month", false)?;
        let (weekdays, weekday_restricted) = field(parts[4], 0, 7, "day-of-week", true)?;
        Ok(Self {
            text: parts.join(" "),
            minutes,
            hours: u32::try_from(hours).unwrap_or(0),
            days: u32::try_from(days >> 1).unwrap_or(0),
            months: u16::try_from(months >> 1).unwrap_or(0),
            weekdays: u8::try_from(weekdays & 0x7f).unwrap_or(0),
            day_restricted,
            weekday_restricted,
        })
    }

    /// The normalised expression.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether the minute starting at `unix_secs` (UTC) matches. A negative time never matches.
    pub fn matches(&self, unix_secs: i64) -> bool {
        let Ok(secs) = u64::try_from(unix_secs) else {
            return false;
        };
        let minute = (secs / 60) % 60;
        let hour = (secs / 3600) % 24;
        let days = secs / 86_400;
        let (_, month, dom) = civil(days);
        let dow = (days + 4) % 7; // 1970-01-01 was a Thursday
        if self.minutes & (1 << minute) == 0
            || self.hours & (1 << hour) == 0
            || self.months & (1 << (month - 1)) == 0
        {
            return false;
        }
        let dom_ok = self.days & (1 << (dom - 1)) != 0;
        let dow_ok = self.weekdays & (1 << dow) != 0;
        match (self.day_restricted, self.weekday_restricted) {
            (true, true) => dom_ok || dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }

    /// The smallest gap in seconds between two matching minutes over the leap year 1972 (so
    /// 29 February is seen). `None` when the expression matches fewer than twice in that year.
    /// Used to refuse schedules that fire more often than the server allows.
    pub fn min_interval_secs(&self) -> Option<u64> {
        // 1972-01-01 starts a leap year, so 29 February is in the window.
        let start = 2 * 365 * 86_400_i64;
        let end = start + 366 * 86_400;
        let mut prev: Option<i64> = None;
        let mut best: Option<u64> = None;
        let mut t = start;
        while t < end {
            if self.matches(t) {
                if let Some(p) = prev {
                    let gap = u64::try_from(t - p).unwrap_or(u64::MAX);
                    best = Some(best.map_or(gap, |b| b.min(gap)));
                }
                prev = Some(t);
            }
            t += 60;
        }
        best
    }
}

/// `(year, month 1..=12, day 1..=31)` of a day count since 1970-01-01.
fn civil(days: u64) -> (i64, u32, u32) {
    // Howard Hinnant's days_from_civil, inverted.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ------------------------------------------------------------------------------ model

/// What starts a workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TriggerKind {
    /// A person (or a token with write access) starts it.
    Manual,
    /// A change appeared on a branch other than trunk.
    ChangeOpened,
    /// A change got a new version.
    ChangeUpdated,
    /// A check of a change failed.
    CheckFailed,
    /// A review comment mentions `@agent-<name>`.
    CommentMention,
    /// A stack landed on a target branch.
    Land,
    /// A cron slot.
    Schedule,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::ChangeOpened => "change_opened",
            Self::ChangeUpdated => "change_updated",
            Self::CheckFailed => "check_failed",
            Self::CommentMention => "comment_mention",
            Self::Land => "land",
            Self::Schedule => "schedule",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "manual" => Self::Manual,
            "change_opened" => Self::ChangeOpened,
            "change_updated" => Self::ChangeUpdated,
            "check_failed" => Self::CheckFailed,
            "comment_mention" => Self::CommentMention,
            "land" => Self::Land,
            "schedule" => Self::Schedule,
            _ => return None,
        })
    }
}

/// One trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// Globs of the target branch (`*` matches any run of characters); empty means every branch.
    pub branches: Vec<String>,
    /// Globs of paths; the trigger fires when the event touches one. Empty means any.
    pub paths: Vec<PathGlob>,
    /// The schedule of a `schedule` trigger.
    pub cron: Option<Cron>,
    /// The agent name a `comment_mention` must carry.
    pub agent: Option<String>,
}

/// An event a trigger is matched against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub kind: Option<TriggerKind>,
    pub branch: String,
    pub paths: Vec<String>,
    /// For a comment mention: the agent names mentioned.
    pub mentions: Vec<String>,
}

impl Trigger {
    /// Whether this trigger fires for `event`. A `schedule` trigger never matches an event; the
    /// scheduler asks [`Cron::matches`].
    pub fn matches(&self, event: &Event) -> bool {
        if event.kind != Some(self.kind) || self.kind == TriggerKind::Schedule {
            return false;
        }
        if !self.branches.is_empty() && !self.branches.iter().any(|g| branch_glob(g, &event.branch))
        {
            return false;
        }
        if !self.paths.is_empty()
            && !event
                .paths
                .iter()
                .any(|p| self.paths.iter().any(|g| g.matches(p.as_bytes())))
        {
            return false;
        }
        if self.kind == TriggerKind::CommentMention
            && let Some(a) = &self.agent
            && !event.mentions.iter().any(|m| m == a)
        {
            return false;
        }
        true
    }
}

/// `*` matches any run of characters (slashes included); everything else is literal and case
/// sensitive, as ref names are.
pub fn branch_glob(pattern: &str, text: &str) -> bool {
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

/// A declared input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub default: String,
}

/// Budget limits; zero means "not stated here" (the policy and the server defaults apply).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub max_changes: u32,
    pub max_tokens: u64,
    pub max_cost_micro_usd: u64,
    pub max_paths: u32,
    pub max_runtime_seconds: u32,
}

/// What a step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StepKind {
    Agent,
    Command,
    Approval,
    Checks,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Approval => "approval",
            Self::Checks => "checks",
        }
    }
}

/// Who may decide an approval step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approver {
    /// `user:<glob>`; `*` matches any run of characters.
    User(String),
    /// `role:<name>` of the repository policy.
    Role(String),
}

impl Approver {
    pub fn render(&self) -> String {
        match self {
            Self::User(g) => format!("user:{g}"),
            Self::Role(r) => format!("role:{r}"),
        }
    }
}

/// One step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    pub needs: Vec<String>,
    pub agent: String,
    pub prompt: String,
    pub paths: Vec<String>,
    pub approvers: Vec<Approver>,
    pub checks: Vec<String>,
    pub command: Vec<String>,
    /// Environment variable to the name of a repository secret.
    pub secrets: BTreeMap<String, String>,
    /// Host names a command that holds secrets may reach; none means no network at all.
    pub egress: Vec<String>,
    pub timeout_seconds: u32,
    pub continue_on_failure: bool,
    pub limits: Limits,
}

impl Step {
    /// The timeout in force: the stated one or the default of the kind.
    pub fn effective_timeout(&self) -> u32 {
        if self.timeout_seconds != 0 {
            return self.timeout_seconds;
        }
        match self.kind {
            StepKind::Agent => DEFAULT_AGENT_TIMEOUT_SECS,
            StepKind::Command => DEFAULT_COMMAND_TIMEOUT_SECS,
            StepKind::Approval => DEFAULT_APPROVAL_TIMEOUT_SECS,
            StepKind::Checks => DEFAULT_CHECKS_TIMEOUT_SECS,
        }
    }
}

/// A parsed, validated workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    /// The file stem.
    pub name: String,
    pub description: String,
    pub triggers: Vec<Trigger>,
    pub inputs: Vec<Input>,
    pub limits: Limits,
    /// 0 in the file means 1.
    pub max_parallel_runs: u32,
    pub steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    #[serde(default)]
    description: String,
    #[serde(default)]
    max_parallel_runs: u32,
    #[serde(default)]
    trigger: Vec<RawTrigger>,
    #[serde(default)]
    input: Vec<RawInput>,
    #[serde(default)]
    budget: RawLimits,
    #[serde(default)]
    step: Vec<RawStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrigger {
    kind: String,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    cron: Option<String>,
    agent: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    #[serde(default)]
    max_changes: u32,
    #[serde(default)]
    max_tokens: u64,
    #[serde(default)]
    max_cost_micro_usd: u64,
    #[serde(default)]
    max_paths: u32,
    #[serde(default)]
    max_runtime_seconds: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    id: String,
    kind: String,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    approvers: Vec<String>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    egress: Vec<String>,
    #[serde(default)]
    timeout_seconds: u32,
    #[serde(default)]
    continue_on_failure: bool,
    #[serde(default)]
    budget: RawLimits,
}

fn limits(raw: &RawLimits, what: &str) -> Result<Limits, WorkflowError> {
    if raw.max_changes > MAX_BUDGET_CHANGES || raw.max_paths > MAX_BUDGET_CHANGES {
        return bad(format!(
            "{what}: max_changes and max_paths are at most {MAX_BUDGET_CHANGES}"
        ));
    }
    if raw.max_runtime_seconds > MAX_TIMEOUT_SECS {
        return bad(format!(
            "{what}: max_runtime_seconds is above {MAX_TIMEOUT_SECS}"
        ));
    }
    Ok(Limits {
        max_changes: raw.max_changes,
        max_tokens: raw.max_tokens,
        max_cost_micro_usd: raw.max_cost_micro_usd,
        max_paths: raw.max_paths,
        max_runtime_seconds: raw.max_runtime_seconds,
    })
}

fn glob_list(list: &[String], what: &str) -> Result<(), WorkflowError> {
    if list.len() > MAX_LIST {
        return bad(format!("{what}: too many patterns"));
    }
    for g in list {
        if g.is_empty() || g.len() > 256 || g.chars().any(char::is_control) {
            return bad(format!(
                "{what}: a pattern is empty, too long or holds a control character"
            ));
        }
    }
    Ok(())
}

fn text_ok(s: &str) -> bool {
    !s.contains('\0')
}

/// The templates `{{...}}` in `text`, as the names they refer to (`input.x`, `change`,
/// `event.f`). An unterminated or empty template is an error.
fn template_names(text: &str) -> Result<Vec<&str>, WorkflowError> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            return bad("a `{{` has no closing `}}`");
        };
        out.push(after[..j].trim());
        rest = &after[j + 2..];
    }
    Ok(out)
}

fn check_template(name: &str, inputs: &BTreeSet<&str>) -> Result<(), WorkflowError> {
    if name == "change" {
        return Ok(());
    }
    if let Some(i) = name.strip_prefix("input.") {
        return if inputs.contains(i) {
            Ok(())
        } else {
            bad(format!(
                "the template {{{{{name}}}}} names an input that is not declared"
            ))
        };
    }
    if let Some(f) = name.strip_prefix("event.") {
        return if EVENT_FIELDS.contains(&f) {
            Ok(())
        } else {
            bad(format!(
                "the template {{{{{name}}}}} names an unknown event field"
            ))
        };
    }
    bad(format!(
        "the template {{{{{name}}}}} is not input.<name>, change or event.<field>"
    ))
}

impl Workflow {
    /// Parses and validates the workflow `name` (the file stem). Strict: an unknown key, a
    /// wrong type, a bad name, glob, cron expression or template, a missing field of a step, an
    /// unknown or cyclic `needs`, or a duplicate id is an error, and nothing partial is returned.
    pub fn parse(name: &str, bytes: &[u8]) -> Result<Self, WorkflowError> {
        if !valid_name(name) {
            return bad("a workflow file is named <name>.toml with a name of [a-z0-9_-]");
        }
        if bytes.len() > MAX_WORKFLOW_BYTES {
            return bad(format!(
                "the workflow file is larger than {MAX_WORKFLOW_BYTES} bytes"
            ));
        }
        let text = std::str::from_utf8(bytes).or_else(|_| bad("the workflow file is not UTF-8"))?;
        let raw: RawWorkflow = toml::from_str(text).map_err(|e| {
            let (line, column) = e.span().map_or((0, 0), |s| position(text, s.start));
            WorkflowError {
                message: format!("workflow.toml: {}", e.message()),
                line,
                column,
            }
        })?;
        Self::build(name, raw)
    }

    fn build(name: &str, raw: RawWorkflow) -> Result<Self, WorkflowError> {
        if raw.description.len() > 1024 || !text_ok(&raw.description) {
            return bad("description is longer than 1024 bytes");
        }
        if raw.max_parallel_runs > 64 {
            return bad("max_parallel_runs is at most 64");
        }
        if raw.trigger.len() > MAX_TRIGGERS {
            return bad(format!("at most {MAX_TRIGGERS} triggers"));
        }
        if raw.input.len() > MAX_INPUTS {
            return bad(format!("at most {MAX_INPUTS} inputs"));
        }
        if raw.step.is_empty() {
            return bad("a workflow needs at least one step");
        }
        if raw.step.len() > MAX_STEPS {
            return bad(format!("at most {MAX_STEPS} steps"));
        }

        let mut inputs = Vec::with_capacity(raw.input.len());
        let mut input_names = BTreeSet::new();
        for i in &raw.input {
            if !valid_name(&i.name) {
                return bad(format!("input name {:?} is not [a-z0-9_-]", i.name));
            }
            if !input_names.insert(i.name.as_str()) {
                return bad(format!("input {:?} is declared twice", i.name));
            }
            if i.description.len() > 512
                || i.default.len() > MAX_INPUT_VALUE_BYTES
                || !text_ok(&i.description)
                || !text_ok(&i.default)
            {
                return bad(format!(
                    "input {:?}: description or default is too long",
                    i.name
                ));
            }
            if i.required && !i.default.is_empty() {
                return bad(format!("input {:?} is required and has a default", i.name));
            }
            inputs.push(Input {
                name: i.name.clone(),
                description: i.description.clone(),
                required: i.required,
                default: i.default.clone(),
            });
        }

        let mut triggers = Vec::with_capacity(raw.trigger.len());
        for t in &raw.trigger {
            let Some(kind) = TriggerKind::parse(&t.kind) else {
                return bad(format!("unknown trigger kind {:?}", t.kind));
            };
            glob_list(&t.branches, "trigger branches")?;
            glob_list(&t.paths, "trigger paths")?;
            let paths = t
                .paths
                .iter()
                .map(|p| {
                    PathGlob::parse(p).map_err(|e| err(format!("trigger path {p:?}: {}", e.0)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let cron = match (&t.cron, kind) {
                (Some(c), TriggerKind::Schedule) => Some(Cron::parse(c)?),
                (None, TriggerKind::Schedule) => return bad("a schedule trigger needs `cron`"),
                (Some(_), _) => return bad("`cron` belongs to schedule triggers only"),
                (None, _) => None,
            };
            if let Some(a) = &t.agent {
                if kind != TriggerKind::CommentMention {
                    return bad("`agent` belongs to comment_mention triggers only");
                }
                if !valid_agent_name(a) {
                    return bad(format!("trigger agent {a:?} is not a valid agent name"));
                }
            }
            if matches!(kind, TriggerKind::Manual | TriggerKind::Schedule) && !t.paths.is_empty() {
                return bad("`paths` does not apply to manual and schedule triggers");
            }
            triggers.push(Trigger {
                kind,
                branches: t.branches.clone(),
                paths,
                cron,
                agent: t.agent.clone(),
            });
        }

        let mut ids = BTreeSet::new();
        for s in &raw.step {
            if !valid_name(&s.id) {
                return bad(format!("step id {:?} is not [a-z0-9_-]", s.id));
            }
            if !ids.insert(s.id.as_str()) {
                return bad(format!("step {:?} is declared twice", s.id));
            }
        }
        let mut steps = Vec::with_capacity(raw.step.len());
        for s in &raw.step {
            steps.push(step(s, &ids, &input_names)?);
        }
        acyclic(&steps)?;

        Ok(Self {
            name: name.to_owned(),
            description: raw.description,
            triggers,
            inputs,
            limits: limits(&raw.budget, "budget")?,
            max_parallel_runs: raw.max_parallel_runs,
            steps,
        })
    }

    /// The step with `id`.
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Checks `given` against the declared inputs and returns the full set with defaults filled
    /// in. Unknown names, a missing required input and an over-long value are errors.
    pub fn resolve_inputs(
        &self,
        given: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, WorkflowError> {
        for k in given.keys() {
            if !self.inputs.iter().any(|i| &i.name == k) {
                return bad(format!("unknown input {k:?}"));
            }
        }
        let mut out = BTreeMap::new();
        for i in &self.inputs {
            let v = match given.get(&i.name) {
                Some(v) => v.clone(),
                None if i.required => return bad(format!("input {:?} is required", i.name)),
                None => i.default.clone(),
            };
            if v.len() > MAX_INPUT_VALUE_BYTES || !text_ok(&v) {
                return bad(format!(
                    "input {:?} is longer than {MAX_INPUT_VALUE_BYTES} bytes",
                    i.name
                ));
            }
            out.insert(i.name.clone(), v);
        }
        Ok(out)
    }

    /// Step ids in an order where every step comes after the steps it needs (stable: ties keep
    /// file order).
    pub fn order(&self) -> Vec<&str> {
        let mut done: Vec<&str> = Vec::with_capacity(self.steps.len());
        while done.len() < self.steps.len() {
            let before = done.len();
            for s in &self.steps {
                if !done.contains(&s.id.as_str())
                    && s.needs.iter().all(|n| done.contains(&n.as_str()))
                {
                    done.push(&s.id);
                }
            }
            if done.len() == before {
                break; // unreachable for a validated workflow
            }
        }
        done
    }
}

fn step(s: &RawStep, ids: &BTreeSet<&str>, inputs: &BTreeSet<&str>) -> Result<Step, WorkflowError> {
    let id = &s.id;
    let ctx = |m: String| err(format!("step {id:?}: {m}"));
    let kind = match s.kind.as_str() {
        "agent" => StepKind::Agent,
        "command" => StepKind::Command,
        "approval" => StepKind::Approval,
        "checks" => StepKind::Checks,
        other => return Err(ctx(format!("unknown kind {other:?}"))),
    };
    if s.needs.len() > MAX_LIST {
        return Err(ctx("too many needs".into()));
    }
    for n in &s.needs {
        if n == id {
            return Err(ctx("needs itself".into()));
        }
        if !ids.contains(n.as_str()) {
            return Err(ctx(format!("needs {n:?}, which is not a step")));
        }
    }
    if s.timeout_seconds > MAX_TIMEOUT_SECS {
        return Err(ctx(format!("timeout_seconds is above {MAX_TIMEOUT_SECS}")));
    }
    let only = |present: bool, key: &str, wanted: &str| {
        if present {
            Err(ctx(format!("`{key}` belongs to {wanted} steps only")))
        } else {
            Ok(())
        }
    };
    let mut agent = String::new();
    let mut prompt = String::new();
    let mut approvers = Vec::new();
    match kind {
        StepKind::Agent => {
            if !valid_agent_name(&s.agent) {
                return Err(ctx(
                    "an agent step needs `agent` (a-z, 0-9, -, at most 40 bytes)".into(),
                ));
            }
            if s.prompt.trim().is_empty() {
                return Err(ctx("an agent step needs a `prompt`".into()));
            }
            if s.prompt.len() > MAX_PROMPT_BYTES || !text_ok(&s.prompt) {
                return Err(ctx(format!(
                    "the prompt is longer than {MAX_PROMPT_BYTES} bytes"
                )));
            }
            for t in template_names(&s.prompt)? {
                check_template(t, inputs).map_err(|e| ctx(e.message))?;
            }
            glob_list(&s.paths, "paths").map_err(|e| ctx(e.message))?;
            for p in &s.paths {
                PathGlob::parse(p).map_err(|e| ctx(format!("path {p:?}: {}", e.0)))?;
            }
            agent.clone_from(&s.agent);
            prompt.clone_from(&s.prompt);
            only(!s.approvers.is_empty(), "approvers", "approval")?;
            only(!s.checks.is_empty(), "checks", "checks")?;
            only(!s.command.is_empty(), "command", "command")?;
            only(!s.secrets.is_empty(), "secrets", "command")?;
            only(!s.egress.is_empty(), "egress", "command")?;
        }
        StepKind::Command => {
            if s.command.is_empty() || s.command.len() > MAX_COMMAND_ARGS {
                return Err(ctx(format!(
                    "a command step needs `command`, 1 to {MAX_COMMAND_ARGS} arguments"
                )));
            }
            for a in &s.command {
                if a.len() > MAX_COMMAND_ARG_BYTES || !text_ok(a) {
                    return Err(ctx(
                        "an argument of `command` is too long or holds a NUL".into()
                    ));
                }
            }
            if s.command[0].is_empty() || s.command[0].contains(['/', '\\']) {
                return Err(ctx(
                    "the program is a bare name (`cargo`), resolved in the sandbox; paths are not allowed".into(),
                ));
            }
            if s.secrets.len() > MAX_LIST {
                return Err(ctx("too many secrets".into()));
            }
            for (env, secret) in &s.secrets {
                if !valid_env_name(env) {
                    return Err(ctx(format!(
                        "{env:?} is not an allowed environment variable name"
                    )));
                }
                if !valid_name(secret) {
                    return Err(ctx(format!("secret name {secret:?} is not [a-z0-9_-]")));
                }
            }
            if s.egress.len() > MAX_LIST {
                return Err(ctx("too many egress hosts".into()));
            }
            for h in &s.egress {
                if !valid_host(h) {
                    return Err(ctx(format!("egress host {h:?} is not a plain host name")));
                }
            }
            only(!s.agent.is_empty(), "agent", "agent")?;
            only(!s.prompt.is_empty(), "prompt", "agent")?;
            only(!s.paths.is_empty(), "paths", "agent")?;
            only(!s.approvers.is_empty(), "approvers", "approval")?;
            only(!s.checks.is_empty(), "checks", "checks")?;
        }
        StepKind::Approval => {
            if s.approvers.is_empty() || s.approvers.len() > MAX_LIST {
                return Err(ctx("an approval step needs `approvers`".into()));
            }
            for a in &s.approvers {
                approvers.push(approver(a).map_err(&ctx)?);
            }
            only(!s.agent.is_empty(), "agent", "agent")?;
            only(!s.prompt.is_empty(), "prompt", "agent")?;
            only(!s.paths.is_empty(), "paths", "agent")?;
            only(!s.checks.is_empty(), "checks", "checks")?;
            only(!s.command.is_empty(), "command", "command")?;
            only(!s.secrets.is_empty(), "secrets", "command")?;
            only(!s.egress.is_empty(), "egress", "command")?;
        }
        StepKind::Checks => {
            if s.checks.is_empty() || s.checks.len() > MAX_LIST {
                return Err(ctx("a checks step needs `checks`".into()));
            }
            for c in &s.checks {
                if c.is_empty() || c.len() > 128 || c.chars().any(char::is_control) {
                    return Err(ctx(
                        "a check name is empty, too long or holds a control character".into(),
                    ));
                }
            }
            only(!s.agent.is_empty(), "agent", "agent")?;
            only(!s.prompt.is_empty(), "prompt", "agent")?;
            only(!s.paths.is_empty(), "paths", "agent")?;
            only(!s.approvers.is_empty(), "approvers", "approval")?;
            only(!s.command.is_empty(), "command", "command")?;
            only(!s.secrets.is_empty(), "secrets", "command")?;
            only(!s.egress.is_empty(), "egress", "command")?;
        }
    }
    let step_limits = limits(&s.budget, &format!("step {id:?} budget"))?;
    if kind != StepKind::Agent && step_limits != Limits::default() {
        return Err(ctx("`[step.budget]` belongs to agent steps only".into()));
    }
    Ok(Step {
        id: id.clone(),
        kind,
        needs: s.needs.clone(),
        agent,
        prompt,
        paths: s.paths.clone(),
        approvers,
        checks: s.checks.clone(),
        command: s.command.clone(),
        secrets: s.secrets.clone(),
        egress: s.egress.clone(),
        timeout_seconds: s.timeout_seconds,
        continue_on_failure: s.continue_on_failure,
        limits: step_limits,
    })
}

/// A lower-case host name: labels of `a-z0-9-` joined by dots, at most 253 bytes. No scheme, port,
/// path or wildcard.
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn approver(text: &str) -> Result<Approver, String> {
    let Some((head, rest)) = text.split_once(':') else {
        return Err(format!(
            "approver {text:?} is not user:<name> or role:<name>"
        ));
    };
    match head {
        "user" => {
            let ok = !rest.is_empty()
                && rest.len() <= 128
                && rest.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+' | b'*')
                });
            if ok {
                Ok(Approver::User(rest.to_owned()))
            } else {
                Err(format!("approver {text:?} has a bad user name"))
            }
        }
        "role" => {
            if valid_name(rest) {
                Ok(Approver::Role(rest.to_owned()))
            } else {
                Err(format!("approver {text:?} has a bad role name"))
            }
        }
        "agent" => Err("an agent never decides an approval".to_owned()),
        _ => Err(format!(
            "approver {text:?} is not user:<name> or role:<name>"
        )),
    }
}

fn acyclic(steps: &[Step]) -> Result<(), WorkflowError> {
    // Kahn's algorithm; what is left over lies on a cycle.
    let mut done: BTreeSet<&str> = BTreeSet::new();
    loop {
        let before = done.len();
        for s in steps {
            if !done.contains(s.id.as_str()) && s.needs.iter().all(|n| done.contains(n.as_str())) {
                done.insert(&s.id);
            }
        }
        if done.len() == steps.len() {
            return Ok(());
        }
        if done.len() == before {
            let stuck: Vec<&str> = steps
                .iter()
                .map(|s| s.id.as_str())
                .filter(|i| !done.contains(i))
                .collect();
            return bad(format!("the steps {stuck:?} need each other in a cycle"));
        }
    }
}

fn position(text: &str, offset: usize) -> (u32, u32) {
    let upto = &text[..offset.min(text.len())];
    let line = upto.bytes().filter(|b| *b == b'\n').count() + 1;
    let col = upto.rsplit('\n').next().map_or(0, str::len) + 1;
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(col).unwrap_or(u32::MAX),
    )
}

// ------------------------------------------------------------------------------ templates

/// The values templates read.
#[derive(Clone, Debug, Default)]
pub struct TemplateContext {
    pub inputs: BTreeMap<String, String>,
    /// The change id the run is about, as text.
    pub change: String,
    pub event: BTreeMap<String, String>,
}

/// Expands `{{input.x}}`, `{{change}}` and `{{event.f}}` in `text`. A missing value renders as
/// the empty string. The result is plain text: the caller must put it in a prompt, never in a
/// command line. Unknown names do not occur in a validated workflow; if one appears it is left
/// as written.
pub fn render(text: &str, ctx: &TemplateContext) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            out.push_str(&rest[i..]);
            return out;
        };
        let name = after[..j].trim();
        let value = if name == "change" {
            Some(ctx.change.as_str())
        } else if let Some(n) = name.strip_prefix("input.") {
            ctx.inputs.get(n).map(String::as_str)
        } else if let Some(f) = name.strip_prefix("event.") {
            if EVENT_FIELDS.contains(&f) {
                Some(ctx.event.get(f).map_or("", String::as_str))
            } else {
                None
            }
        } else {
            None
        };
        match value {
            Some(v) => out.push_str(v),
            None => {
                out.push_str("{{");
                out.push_str(&after[..j]);
                out.push_str("}}");
            }
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

/// The environment variables a command step sees for its run: `ASC_CHANGE`, `ASC_INPUT_<NAME>`
/// and `ASC_EVENT_<FIELD>`. Names are upper-cased with `-` as `_`.
pub fn command_env(ctx: &TemplateContext) -> BTreeMap<String, String> {
    let up = |s: &str| s.to_ascii_uppercase().replace('-', "_");
    let mut env = BTreeMap::new();
    env.insert("ASC_CHANGE".to_owned(), ctx.change.clone());
    for (k, v) in &ctx.inputs {
        env.insert(format!("ASC_INPUT_{}", up(k)), v.clone());
    }
    for (k, v) in &ctx.event {
        env.insert(format!("ASC_EVENT_{}", up(k)), v.clone());
    }
    env
}

/// Whether `who_name` (a user handle) is admitted by `approvers`. `is_in_role` answers the
/// `role:<name>` entries from the repository policy.
pub fn approver_admits(
    approvers: &[Approver],
    who_name: &str,
    is_in_role: impl Fn(&str) -> bool,
) -> bool {
    approvers.iter().any(|a| match a {
        Approver::User(g) => branch_glob(g, who_name),
        Approver::Role(r) => is_in_role(r),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
description = "fix and prove"
max_parallel_runs = 2

[[trigger]]
kind = "check_failed"
branches = ["main", "release/*"]
paths = ["src/**"]

[[trigger]]
kind = "schedule"
cron = "0 3 * * 1-5"

[[trigger]]
kind = "comment_mention"
agent = "fixer"

[[input]]
name = "ticket"
default = "none"

[budget]
max_changes = 5
max_tokens = 100000

[[step]]
id = "fix"
kind = "agent"
agent = "fixer"
prompt = "Fix {{event.check}} on {{change}} for {{input.ticket}}"
paths = ["src/**"]
timeout_seconds = 600
[step.budget]
max_tokens = 50000

[[step]]
id = "test"
kind = "command"
needs = ["fix"]
command = ["cargo", "test"]
secrets = { REGISTRY_TOKEN = "registry_token" }

[[step]]
id = "ok"
kind = "approval"
needs = ["test"]
approvers = ["role:maintainer", "user:al*"]

[[step]]
id = "ci"
kind = "checks"
needs = ["ok"]
checks = ["build"]
continue_on_failure = true
"#;

    fn parse(text: &str) -> Result<Workflow, WorkflowError> {
        Workflow::parse("triage", text.as_bytes())
    }

    fn step_with(extra: &str) -> String {
        format!("[[step]]\nid = \"a\"\n{extra}")
    }

    #[test]
    fn a_full_workflow_parses() {
        let w = parse(GOOD).unwrap();
        assert_eq!(w.name, "triage");
        assert_eq!(w.triggers.len(), 3);
        assert_eq!(w.steps.len(), 4);
        assert_eq!(w.limits.max_changes, 5);
        assert_eq!(w.steps[0].limits.max_tokens, 50_000);
        assert_eq!(w.steps[1].command, ["cargo", "test"]);
        assert_eq!(w.steps[1].secrets["REGISTRY_TOKEN"], "registry_token");
        assert_eq!(w.steps[2].approvers.len(), 2);
        assert_eq!(w.order(), ["fix", "test", "ok", "ci"]);
        assert_eq!(w.steps[0].effective_timeout(), 600);
        assert_eq!(w.steps[1].effective_timeout(), DEFAULT_COMMAND_TIMEOUT_SECS);
    }

    #[test]
    fn unknown_keys_are_errors_at_every_level() {
        for (extra, what) in [
            ("bogus = 1\n", "top"),
            ("[[trigger]]\nkind = \"manual\"\nbogus = 1\n", "trigger"),
            ("[[input]]\nname = \"a\"\nbogus = 1\n", "input"),
            ("[budget]\nbogus = 1\n", "budget"),
        ] {
            let text = format!(
                "{extra}{}",
                step_with("kind = \"approval\"\napprovers = [\"role:m\"]\n")
            );
            // `bogus` at the top has to precede the tables.
            let text = if what == "top" {
                format!("bogus = 1\n{}", text.trim_start_matches("bogus = 1\n"))
            } else {
                text
            };
            assert!(parse(&text).is_err(), "{what}");
        }
        assert!(
            parse(&step_with(
                "kind = \"approval\"\napprovers = [\"role:m\"]\nbogus = 1\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn the_bounds_hold() {
        let steps: String = (0..=MAX_STEPS)
            .map(|i| {
                format!("[[step]]\nid = \"s{i}\"\nkind = \"approval\"\napprovers = [\"role:m\"]\n")
            })
            .collect();
        assert!(parse(&steps).is_err());
        let ok: String = (0..MAX_STEPS)
            .map(|i| {
                format!("[[step]]\nid = \"s{i}\"\nkind = \"approval\"\napprovers = [\"role:m\"]\n")
            })
            .collect();
        assert_eq!(parse(&ok).unwrap().steps.len(), MAX_STEPS);

        let triggers: String = (0..=MAX_TRIGGERS)
            .map(|_| "[[trigger]]\nkind = \"manual\"\n")
            .collect();
        assert!(
            parse(&format!(
                "{triggers}{}",
                step_with("kind = \"approval\"\napprovers = [\"role:m\"]\n")
            ))
            .is_err()
        );

        let big_prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert!(
            parse(&step_with(&format!(
                "kind = \"agent\"\nagent = \"a\"\nprompt = {big_prompt:?}\n"
            )))
            .is_err()
        );
        let fits = "x".repeat(MAX_PROMPT_BYTES);
        assert!(
            parse(&step_with(&format!(
                "kind = \"agent\"\nagent = \"a\"\nprompt = {fits:?}\n"
            )))
            .is_ok()
        );

        let huge = format!("description = \"{}\"\n", "d".repeat(MAX_WORKFLOW_BYTES));
        assert!(parse(&huge).is_err());
        assert!(Workflow::parse("Bad Name", GOOD.as_bytes()).is_err());
        assert!(parse("").is_err(), "no steps");
    }

    #[test]
    fn egress_names_plain_hosts_on_command_steps_only() {
        let ok = step_with(
            "kind = \"command\"
command = [\"ls\"]
egress = [\"registry.example.com\", \"a-b.io\"]
",
        );
        assert_eq!(
            parse(&ok).unwrap().steps[0].egress,
            ["registry.example.com", "a-b.io"]
        );
        for bad in [
            "egress = [\"https://x.io\"]",
            "egress = [\"x.io:443\"]",
            "egress = [\"*.x.io\"]",
            "egress = [\"X.io\"]",
            "egress = [\"\"]",
        ] {
            let t = step_with(&format!(
                "kind = \"command\"
command = [\"ls\"]
{bad}
"
            ));
            assert!(parse(&t).is_err(), "{bad}");
        }
        let elsewhere = step_with(
            "kind = \"approval\"
approvers = [\"role:m\"]
egress = [\"x.io\"]
",
        );
        assert!(parse(&elsewhere).is_err());
    }

    #[test]
    fn needs_must_name_steps_and_not_cycle() {
        let a = "[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:m\"]\nneeds = [\"b\"]\n";
        let b = "[[step]]\nid = \"b\"\nkind = \"approval\"\napprovers = [\"role:m\"]\nneeds = [\"a\"]\n";
        let e = parse(&format!("{a}{b}")).unwrap_err();
        assert!(e.message.contains("cycle"), "{e}");
        let me = "[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:m\"]\nneeds = [\"a\"]\n";
        assert!(parse(me).unwrap_err().message.contains("itself"));
        let missing = "[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:m\"]\nneeds = [\"zzz\"]\n";
        assert!(parse(missing).unwrap_err().message.contains("not a step"));
        let dup =
            format!("{a}[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:m\"]\n");
        assert!(parse(&dup).unwrap_err().message.contains("twice"));
    }

    #[test]
    fn step_kinds_take_only_their_own_keys() {
        for text in [
            // an agent step with a command
            step_with("kind = \"agent\"\nagent = \"a\"\nprompt = \"p\"\ncommand = [\"ls\"]\n"),
            // an agent without a prompt
            step_with("kind = \"agent\"\nagent = \"a\"\n"),
            // a bad agent name
            step_with("kind = \"agent\"\nagent = \"A b\"\nprompt = \"p\"\n"),
            // a command with a path program
            step_with("kind = \"command\"\ncommand = [\"/bin/sh\", \"-c\", \"x\"]\n"),
            step_with("kind = \"command\"\ncommand = [\"..\\\\x\"]\n"),
            // an empty command
            step_with("kind = \"command\"\n"),
            // a reserved env name
            step_with("kind = \"command\"\ncommand = [\"ls\"]\nsecrets = { PATH = \"x\" }\n"),
            step_with("kind = \"command\"\ncommand = [\"ls\"]\nsecrets = { ASC_X = \"x\" }\n"),
            // an agent approver
            step_with("kind = \"approval\"\napprovers = [\"agent:bot\"]\n"),
            step_with("kind = \"approval\"\n"),
            step_with("kind = \"checks\"\n"),
            step_with("kind = \"nope\"\n"),
            // a budget on a command
            step_with("kind = \"command\"\ncommand = [\"ls\"]\n[step.budget]\nmax_tokens = 1\n"),
            // a timeout past the cap
            step_with(&format!(
                "kind = \"approval\"\napprovers = [\"role:m\"]\ntimeout_seconds = {}\n",
                MAX_TIMEOUT_SECS + 1
            )),
        ] {
            assert!(parse(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn templates_are_checked_and_rendered_as_text() {
        assert!(
            parse(&step_with(
                "kind = \"agent\"\nagent = \"a\"\nprompt = \"{{input.nope}}\"\n"
            ))
            .is_err()
        );
        assert!(
            parse(&step_with(
                "kind = \"agent\"\nagent = \"a\"\nprompt = \"{{event.nope}}\"\n"
            ))
            .is_err()
        );
        assert!(
            parse(&step_with(
                "kind = \"agent\"\nagent = \"a\"\nprompt = \"{{whatever}}\"\n"
            ))
            .is_err()
        );
        assert!(
            parse(&step_with(
                "kind = \"agent\"\nagent = \"a\"\nprompt = \"{{change\"\n"
            ))
            .is_err()
        );
        let mut ctx = TemplateContext::default();
        ctx.inputs.insert("ticket".into(), "T-1 {{change}}".into());
        ctx.change = "abc".into();
        ctx.event.insert("check".into(), "build".into());
        // A value is inserted once, as text; a template inside a value is not expanded again.
        assert_eq!(
            render(
                "a {{input.ticket}} b {{ change }} c {{event.check}} d {{event.branch}}.",
                &ctx
            ),
            "a T-1 {{change}} b abc c build d ."
        );
        assert_eq!(render("{{ nothing", &ctx), "{{ nothing");
        assert_eq!(render("{{x}}", &ctx), "{{x}}");
    }

    #[test]
    fn command_steps_see_event_values_only_through_the_environment() {
        let w = parse(&step_with(
            "kind = \"command\"\ncommand = [\"echo\", \"{{input.x}}\"]\n",
        ))
        .unwrap();
        // Not expanded: the argument stays as written.
        assert_eq!(w.steps[0].command[1], "{{input.x}}");
        let mut ctx = TemplateContext::default();
        ctx.inputs.insert("my-input".into(), "v; rm -rf /".into());
        ctx.change = "c1".into();
        ctx.event.insert("branch".into(), "main".into());
        let env = command_env(&ctx);
        assert_eq!(env["ASC_INPUT_MY_INPUT"], "v; rm -rf /");
        assert_eq!(env["ASC_CHANGE"], "c1");
        assert_eq!(env["ASC_EVENT_BRANCH"], "main");
    }

    #[test]
    fn inputs_are_resolved_strictly() {
        let w = parse(&format!(
            "[[input]]\nname = \"need\"\nrequired = true\n[[input]]\nname = \"opt\"\ndefault = \"d\"\n{}",
            step_with("kind = \"approval\"\napprovers = [\"role:m\"]\n")
        ))
        .unwrap();
        let mut given = BTreeMap::new();
        assert!(w.resolve_inputs(&given).is_err(), "required missing");
        given.insert("need".to_owned(), "1".to_owned());
        let r = w.resolve_inputs(&given).unwrap();
        assert_eq!(r["opt"], "d");
        given.insert("zzz".to_owned(), "1".to_owned());
        assert!(w.resolve_inputs(&given).is_err(), "unknown");
        given.remove("zzz");
        given.insert("need".to_owned(), "x".repeat(MAX_INPUT_VALUE_BYTES + 1));
        assert!(w.resolve_inputs(&given).is_err(), "too long");
        // A required input cannot carry a default.
        assert!(
            parse(&format!(
                "[[input]]\nname = \"n\"\nrequired = true\ndefault = \"x\"\n{}",
                step_with("kind = \"approval\"\napprovers = [\"role:m\"]\n")
            ))
            .is_err()
        );
    }

    #[test]
    fn trigger_matching_follows_kind_branch_path_and_mention() {
        let w = parse(GOOD).unwrap();
        let failed = |branch: &str, paths: &[&str]| Event {
            kind: Some(TriggerKind::CheckFailed),
            branch: branch.into(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            mentions: vec![],
        };
        let t = &w.triggers[0];
        assert!(t.matches(&failed("main", &["src/a.rs"])));
        assert!(t.matches(&failed("release/1.2", &["docs/x", "SRC/deep/a.rs"])));
        assert!(!t.matches(&failed("dev", &["src/a.rs"])));
        assert!(!t.matches(&failed("main", &["docs/a.md"])));
        assert!(!t.matches(&Event {
            kind: Some(TriggerKind::Land),
            branch: "main".into(),
            paths: vec!["src/a".into()],
            mentions: vec![]
        }));
        assert!(
            !w.triggers[1].matches(&failed("main", &[])),
            "a schedule never matches an event"
        );
        let mention = |who: &str| Event {
            kind: Some(TriggerKind::CommentMention),
            mentions: vec![who.to_owned()],
            ..Event::default()
        };
        assert!(w.triggers[2].matches(&mention("fixer")));
        assert!(!w.triggers[2].matches(&mention("other")));
    }

    #[test]
    fn trigger_keys_are_checked() {
        let step = step_with("kind = \"approval\"\napprovers = [\"role:m\"]\n");
        for t in [
            "kind = \"schedule\"",                         // no cron
            "kind = \"manual\"\ncron = \"* * * * *\"",     // cron on manual
            "kind = \"land\"\nagent = \"x\"",              // agent on land
            "kind = \"comment_mention\"\nagent = \"X Y\"", // bad agent
            "kind = \"manual\"\npaths = [\"a\"]",          // paths on manual
            "kind = \"nope\"",
            "kind = \"land\"\nbranches = [\"\"]",
        ] {
            assert!(parse(&format!("[[trigger]]\n{t}\n{step}")).is_err(), "{t}");
        }
    }

    #[test]
    fn cron_parses_matches_and_measures_its_interval() {
        // 2026-10-26 is a Monday. 03:00 UTC that day:
        let monday_3 = 1_792_983_600_i64; // computed below
        let day = u64::try_from(monday_3).unwrap() / 86_400;
        assert_eq!((day + 4) % 7, 1, "Monday");
        let c = Cron::parse("0 3 * * 1-5").unwrap();
        assert!(c.matches(monday_3));
        assert!(!c.matches(monday_3 + 60));
        assert!(!c.matches(monday_3 + 86_400 * 5)); // Saturday
        assert!(c.matches(monday_3 + 86_400 * 4)); // Friday
        assert_eq!(c.min_interval_secs(), Some(86_400)); // Fri 03:00 -> Mon 03:00 is longer; weekdays are daily
        let every15 = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(every15.min_interval_secs(), Some(900));
        assert!(every15.matches(monday_3 + 15 * 60));
        assert!(!every15.matches(monday_3 + 16 * 60));
        assert_eq!(
            Cron::parse("* * * * *").unwrap().min_interval_secs(),
            Some(60)
        );
        // Sunday as 0 and as 7.
        let sunday = monday_3 - 86_400;
        assert!(Cron::parse("0 3 * * 0").unwrap().matches(sunday));
        assert!(Cron::parse("0 3 * * 7").unwrap().matches(sunday));
        // Day-of-month or day-of-week when both are restricted (POSIX).
        let either = Cron::parse("0 3 26 * 0").unwrap();
        assert!(either.matches(monday_3), "the 26th");
        assert!(either.matches(sunday), "a Sunday");
        assert!(!either.matches(monday_3 + 86_400), "the 27th is a Tuesday");
        // Monthly: the shortest gap in the window is February 1972, which has 29 days.
        assert_eq!(
            Cron::parse("0 0 1 * *").unwrap().min_interval_secs(),
            Some(29 * 86_400)
        );
        // 29 February exists in the window, once a year.
        assert_eq!(Cron::parse("0 0 29 2 *").unwrap().min_interval_secs(), None);
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "@daily",
            "a * * * *",
            "1-2-3 * * * *",
            "-1 * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            Cron::parse("  0   3 *  * 1,3,5 ").unwrap().as_str(),
            "0 3 * * 1,3,5"
        );
    }

    #[test]
    fn civil_dates_are_right() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(365), (1971, 1, 1));
        assert_eq!(civil(59), (1970, 3, 1));
        assert_eq!(civil(789), (1972, 2, 29));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }

    #[test]
    fn toml_errors_carry_a_line() {
        let e = parse("description = \"x\"\n[[step]]\nid = 5\n").unwrap_err();
        assert_eq!(e.line, 3, "{e:?}");
        assert!(parse("\u{0}").is_err());
        assert!(Workflow::parse("triage", &[0xff, 0xfe]).is_err());
    }

    #[test]
    fn approvers_admit_users_and_roles() {
        let w = parse(GOOD).unwrap();
        let a = &w.steps[2].approvers;
        assert!(approver_admits(a, "alice", |_| false), "user:al*");
        assert!(!approver_admits(a, "bob", |_| false));
        assert!(approver_admits(a, "bob", |r| r == "maintainer"));
        assert!(!approver_admits(a, "bob", |r| r == "other"));
    }
}
