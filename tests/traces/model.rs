//! The `.trace` file format (docs/SPEC.md §17.1) and its validation.
//!
//! A trace is TOML: header keys, then `[[step]]` entries in order. A step is exactly one of
//!
//! * `git = [argv…]` with `exit`, optional `env`, `stdin`, `routes = "real"`, and the `[[step.hook]]`
//!   tables of the git hooks that ran during it (each with `[[step.hook.call]]` entries);
//! * `sh = "…"`, a non-git action (a file edit);
//! * `hook_event = "…"`, a Claude Code hook event that the git runner skips (it pins the JSON a
//!   asc hook must accept).
//!
//! `${ROOT}` stands for the fixture's work tree root in `git`, `env`, `sh` and `stdin` text.
//! Validation fails closed: unknown keys, absolute host paths, unlisted environment variables
//! and anything over a named limit are errors.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Largest `.trace` file read.
pub const MAX_TRACE_BYTES: u64 = 1024 * 1024;
/// Most steps in one trace.
pub const MAX_STEPS: usize = 2_000;
/// Most hook calls in one hook.
pub const MAX_HOOK_CALLS: usize = 200;
/// Most arguments in one argv.
pub const MAX_ARGS: usize = 256;
/// Longest single argument or snippet, in bytes.
pub const MAX_TEXT: usize = 64 * 1024;

/// The git hooks (SPEC §9.4.1) a step may carry.
pub const HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-auto-gc",
    "post-rewrite",
    "reference-transaction",
];

/// Environment variables a trace may set on a call. Anything else (`PATH`, `GIT_EXEC_PATH`,
/// `GIT_CONFIG_*`, `GIT_SSH*`, `GIT_ASKPASS`, `HOME`…) would let a trace redirect the program
/// under test, so it is refused.
pub const ENV_ALLOWED: &[&str] = &[
    "LC_ALL",
    "LANG",
    "GIT_INDEX_FILE",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
    "GIT_EDITOR",
    "GIT_REFLOG_ACTION",
    "GIT_OPTIONAL_LOCKS",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_DATE",
];

/// How a trace came to be.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// Captured by `record/record.sh` from the pinned tool running against real git.
    Recorded,
    /// Written by hand from the tool's documented or source-level git usage; names the spike
    /// (`recapture`) that replaces it with a recording.
    Authored,
    /// The argv taken from a repository script's source; names the file (`source`).
    Extracted,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trace {
    pub tool: String,
    /// The pinned version of the tool (`pins.toml`).
    pub version: String,
    pub provenance: Provenance,
    /// The git version the exit codes were recorded or written against.
    pub git: String,
    /// The scripted user action that drove the tool.
    pub driver: String,
    pub recapture: Option<String>,
    pub source: Option<String>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub git: Option<Vec<String>>,
    pub sh: Option<String>,
    pub hook_event: Option<String>,
    /// Who ran it: `driver` (the scripted user), the tool, or `hook:<Event>`.
    pub from: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub exit: Option<i32>,
    pub stdin: Option<String>,
    /// The stdout a `hook_event` must produce.
    pub stdout: Option<String>,
    /// `real`: the shim hands this invocation to real git (SPEC §11.4), so it is outside the
    /// native table by design.
    pub routes: Option<String>,
    #[serde(default, rename = "hook")]
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub name: String,
    /// Where the hook script lives, relative to the work tree root.
    pub path: String,
    pub exit: i32,
    #[serde(default, rename = "call")]
    pub calls: Vec<Call>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub git: Option<Vec<String>>,
    pub sh: Option<String>,
    pub from: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub exit: Option<i32>,
    /// As on [`Step`].
    pub routes: Option<String>,
}

impl Trace {
    pub fn parse(name: &str, text: &str) -> Result<Trace, String> {
        let trace: Trace = toml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
        trace.validate().map_err(|e| format!("{name}: {e}"))?;
        Ok(trace)
    }

    fn validate(&self) -> Result<(), String> {
        for (what, v) in [
            ("tool", &self.tool),
            ("version", &self.version),
            ("git", &self.git),
            ("driver", &self.driver),
        ] {
            if v.trim().is_empty() {
                return Err(format!("`{what}` is empty"));
            }
        }
        match self.provenance {
            Provenance::Authored if self.recapture.is_none() => {
                return Err(
                    "an authored trace names the spike that replaces it (`recapture`)".into(),
                );
            }
            Provenance::Extracted if self.source.is_none() => {
                return Err("an extracted trace names its `source` file".into());
            }
            _ => {}
        }
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err(format!(
                "{} steps (1..={MAX_STEPS} allowed)",
                self.steps.len()
            ));
        }
        for (i, step) in self.steps.iter().enumerate() {
            step.validate()
                .map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        Ok(())
    }
}

impl Step {
    fn validate(&self) -> Result<(), String> {
        let kinds = [
            self.git.is_some(),
            self.sh.is_some(),
            self.hook_event.is_some(),
        ];
        if kinds.iter().filter(|k| **k).count() != 1 {
            return Err("a step is exactly one of `git`, `sh`, `hook_event`".into());
        }
        if self.from.trim().is_empty() {
            return Err("`from` is empty".into());
        }
        check_env(&self.env)?;
        if let Some(argv) = &self.git {
            check_argv(argv)?;
            match self.exit {
                Some(0..=255) => {}
                other => return Err(format!("`exit` must be 0..=255, got {other:?}")),
            }
            if let Some(stdin) = &self.stdin {
                check_text(stdin)?;
            }
            check_routes(self.routes.as_deref())?;
        } else if !self.hooks.is_empty()
            || self.exit.is_some()
            || self.routes.is_some()
            || !self.env.is_empty()
        {
            return Err("`hook`, `exit`, `routes` and `env` belong to git steps".into());
        }
        if let Some(sh) = &self.sh {
            check_text(sh)?;
        }
        if self.hook_event.is_some() {
            let Some(stdin) = &self.stdin else {
                return Err("a hook_event carries its `stdin` JSON".into());
            };
            serde_json::from_str::<serde_json::Value>(&stdin.replace("${ROOT}", "/root"))
                .map_err(|e| format!("hook_event stdin is not JSON: {e}"))?;
        } else if self.stdout.is_some() {
            return Err("`stdout` belongs to hook_event steps".into());
        }
        if self.git.is_none() && self.stdin.is_some() && self.hook_event.is_none() {
            return Err("`stdin` belongs to git and hook_event steps".into());
        }
        if self.hooks.len() > HOOKS.len() * 4 {
            return Err("too many hooks".into());
        }
        for hook in &self.hooks {
            hook.validate()?;
        }
        Ok(())
    }
}

impl Hook {
    fn validate(&self) -> Result<(), String> {
        if !HOOKS.contains(&self.name.as_str()) {
            return Err(format!(
                "hook {:?} is not a git hook (SPEC §9.4.1)",
                self.name
            ));
        }
        check_relative(self.path.strip_prefix("${GITDIR}/").unwrap_or(&self.path))
            .map_err(|e| format!("hook {} path: {e}", self.name))?;
        if !(0..=255).contains(&self.exit) {
            return Err(format!("hook {} exit {}", self.name, self.exit));
        }
        if self.calls.is_empty() || self.calls.len() > MAX_HOOK_CALLS {
            return Err(format!("hook {} has {} calls", self.name, self.calls.len()));
        }
        for call in &self.calls {
            if call.from.trim().is_empty() {
                return Err(format!("hook {}: a call has an empty `from`", self.name));
            }
            if call.git.is_some() == call.sh.is_some() {
                return Err(format!("hook {}: a call is `git` or `sh`", self.name));
            }
            check_env(&call.env)?;
            if let Some(argv) = &call.git {
                check_argv(argv)?;
                if !matches!(call.exit, Some(0..=255)) {
                    return Err(format!(
                        "hook {}: a git call needs `exit` 0..=255",
                        self.name
                    ));
                }
                check_routes(call.routes.as_deref())?;
            }
            if let Some(sh) = &call.sh {
                check_text(sh)?;
            }
        }
        Ok(())
    }
}

fn check_routes(routes: Option<&str>) -> Result<(), String> {
    match routes {
        None | Some("real") => Ok(()),
        Some(other) => Err(format!("`routes` is `real` or absent, got {other:?}")),
    }
}

fn check_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    for (k, v) in env {
        if !ENV_ALLOWED.contains(&k.as_str()) {
            return Err(format!(
                "environment variable {k} is not allowed in a trace"
            ));
        }
        check_text(v)?;
    }
    Ok(())
}

fn check_text(s: &str) -> Result<(), String> {
    if s.len() > MAX_TEXT {
        return Err(format!(
            "text of {} bytes exceeds the {MAX_TEXT} byte limit",
            s.len()
        ));
    }
    if s.contains('\0') {
        return Err("text holds a NUL".into());
    }
    if s.contains("ASC_TRACE_HOOK_EOF") {
        return Err("text holds the replay's here-document marker".into());
    }
    Ok(())
}

fn check_argv(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv.len() > MAX_ARGS {
        return Err(format!(
            "argv of {} words (1..={MAX_ARGS} allowed)",
            argv.len()
        ));
    }
    for arg in argv {
        check_text(arg)?;
        check_arg_path(arg)?;
    }
    Ok(())
}

/// An argument must not name a host path: the fixture root is `${ROOT}`, and the only other
/// place a trace may touch is a sibling bare repository, `../<name>.git`.
fn check_arg_path(arg: &str) -> Result<(), String> {
    let rooted = arg
        .strip_prefix("${ROOT}")
        .or_else(|| arg.strip_prefix("${GITDIR}"));
    if rooted.is_none() && (arg.starts_with('/') || arg.starts_with('\\') || has_drive(arg)) {
        return Err(format!("argument {arg:?} is a host path; use ${{ROOT}}"));
    }
    if arg.contains('\\') && !arg.contains(' ') && !arg.contains('\n') {
        return Err(format!("argument {arg:?} has a backslash path separator"));
    }
    let path = rooted.unwrap_or(arg);
    if path.split('/').any(|c| c == "..") && !is_sibling_repo(path) {
        return Err(format!("argument {arg:?} climbs out of the work tree"));
    }
    Ok(())
}

fn has_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// `../origin.git`: the one escape a trace may use, a bare repository next to the work tree.
fn is_sibling_repo(path: &str) -> bool {
    path.strip_prefix("../").is_some_and(|name| {
        name.ends_with(".git")
            && name.len() > 4
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    })
}

fn check_relative(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || has_drive(path) {
        return Err(format!("{path:?} is not a relative `/` path"));
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(format!("{path:?} has an empty, `.` or `..` component"));
    }
    Ok(())
}
