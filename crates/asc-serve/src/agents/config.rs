//! `[agents]` of `asc-serve.toml`.

use std::path::PathBuf;

use serde::Deserialize;

/// The limits that apply when a request, a token and the repository policy state none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DefaultLimits {
    pub max_changes: u32,
    pub max_tokens: u64,
    pub max_cost_micro_usd: u64,
    pub max_paths: u32,
    pub max_runtime_seconds: u32,
}

impl Default for DefaultLimits {
    fn default() -> Self {
        Self {
            max_changes: 20,
            max_tokens: 1_000_000,
            max_cost_micro_usd: 5_000_000,
            max_paths: 0,
            max_runtime_seconds: 3600,
        }
    }
}

/// `[agents.sandbox]`: the commands an agent or a workflow step may run on a working copy.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxConfig {
    /// Allowed argument-list prefixes, each a list: `[["cargo", "test"], ["npm", "test"]]`.
    /// Empty (the default) means no command can run.
    pub allow: Vec<Vec<String>>,
    /// Absolute directories programs are found in; the child's `PATH`.
    pub path_dirs: Vec<PathBuf>,
    /// Environment variables copied from the server into the child (`CARGO_HOME`, ...).
    pub pass_env: Vec<String>,
    /// The isolation wrapper put in front of every command (`bwrap`, `firejail`, `docker run`,
    /// `prlimit`); `{cwd}` is replaced by the working directory.
    pub wrapper: Vec<String>,
    /// Run commands with no wrapper. Off: the server refuses to run a command without isolation.
    pub allow_unisolated: bool,
    /// Longest output captured from a command, in bytes.
    pub max_output_bytes: usize,
    /// Longest any command runs, in seconds.
    pub max_command_seconds: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            path_dirs: Vec::new(),
            pass_env: Vec::new(),
            wrapper: Vec::new(),
            allow_unisolated: false,
            max_output_bytes: 256 * 1024,
            max_command_seconds: 1800,
        }
    }
}

/// `[agents]`. Off unless `enabled`; safe when absent.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentsConfig {
    /// The instance switch. A repository's policy `[agents] enabled` must be on as well.
    pub enabled: bool,
    /// Longest an agent token lives, in seconds (90 days).
    pub max_token_ttl_secs: u64,
    pub default_limits: DefaultLimits,
    /// Runs executing at once on this server.
    pub max_concurrent_runs: u32,
    /// Runs of one repository executing at once.
    pub max_concurrent_per_repo: u32,
    /// Longest task text of a run, in bytes.
    pub max_task_bytes: usize,
    /// Days run events are kept.
    pub event_retention_days: u32,
    /// Where working copies are made. Default: `asc-agents` in the system temporary directory.
    pub work_dir: Option<PathBuf>,
    /// Most files and bytes a working copy may hold.
    pub max_work_files: usize,
    pub max_work_bytes: u64,
    /// Largest file a tool reads or writes, in bytes.
    pub max_file_bytes: u64,
    /// Most model turns in one run.
    pub max_steps: u32,
    /// Most tokens the model may write in one answer.
    pub max_output_tokens: u32,
    pub sandbox: SandboxConfig,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_token_ttl_secs: 90 * 86_400,
            default_limits: DefaultLimits::default(),
            max_concurrent_runs: 4,
            max_concurrent_per_repo: 1,
            max_task_bytes: 65_536,
            event_retention_days: 30,
            work_dir: None,
            max_work_files: 20_000,
            max_work_bytes: 512 * 1024 * 1024,
            max_file_bytes: 4 * 1024 * 1024,
            max_steps: 200,
            max_output_tokens: 8192,
            sandbox: SandboxConfig::default(),
        }
    }
}

impl AgentsConfig {
    /// Checks the bounds; the message names the key.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |m: &str| Err(m.to_owned());
        if self.max_token_ttl_secs == 0 || self.max_token_ttl_secs > 366 * 86_400 {
            return bad("agents.max_token_ttl_secs must be 1 to 31622400");
        }
        if self.max_concurrent_runs == 0 || self.max_concurrent_runs > 256 {
            return bad("agents.max_concurrent_runs must be 1 to 256");
        }
        if self.max_concurrent_per_repo == 0
            || self.max_concurrent_per_repo > self.max_concurrent_runs
        {
            return bad("agents.max_concurrent_per_repo must be 1 to max_concurrent_runs");
        }
        if self.max_task_bytes == 0 || self.max_task_bytes > 1024 * 1024 {
            return bad("agents.max_task_bytes must be 1 to 1048576");
        }
        if self.max_steps == 0 || self.max_steps > 10_000 {
            return bad("agents.max_steps must be 1 to 10000");
        }
        if self.max_output_tokens == 0 || self.max_output_tokens > 200_000 {
            return bad("agents.max_output_tokens must be 1 to 200000");
        }
        if self.max_work_files == 0 || self.max_work_bytes == 0 || self.max_file_bytes == 0 {
            return bad(
                "agents.max_work_files, max_work_bytes and max_file_bytes must be positive",
            );
        }
        if self.max_file_bytes > self.max_work_bytes {
            return bad("agents.max_file_bytes is above max_work_bytes");
        }
        if self.default_limits.max_runtime_seconds == 0 {
            return bad("agents.default_limits.max_runtime_seconds must be positive");
        }
        let s = &self.sandbox;
        if s.allow
            .iter()
            .any(|a| a.is_empty() || a.iter().any(|x| x.is_empty() || x.contains('\0')))
        {
            return bad("agents.sandbox.allow entries are non-empty argument lists");
        }
        if s.path_dirs.iter().any(|d| !d.is_absolute()) {
            return bad("agents.sandbox.path_dirs must be absolute");
        }
        if s.max_output_bytes == 0 || s.max_command_seconds == 0 {
            return bad("agents.sandbox.max_output_bytes and max_command_seconds must be positive");
        }
        if !s.allow.is_empty() && s.path_dirs.is_empty() {
            return bad("agents.sandbox.allow needs agents.sandbox.path_dirs");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_off() {
        let c = AgentsConfig::default();
        assert!(!c.enabled);
        assert!(c.validate().is_ok());
        assert_eq!(c.default_limits.max_changes, 20);
        assert_eq!(c.default_limits.max_tokens, 1_000_000);
        assert_eq!(c.default_limits.max_cost_micro_usd, 5_000_000);
        assert_eq!(c.default_limits.max_runtime_seconds, 3600);
        assert_eq!((c.max_concurrent_runs, c.max_concurrent_per_repo), (4, 1));
        assert_eq!(c.max_token_ttl_secs, 7_776_000);
    }

    #[test]
    fn parsing_is_strict() {
        assert!(toml::from_str::<AgentsConfig>("bogus = 1").is_err());
        assert!(toml::from_str::<AgentsConfig>("[sandbox]\nbogus = 1").is_err());
        let dir = if cfg!(windows) {
            r"C:\tools"
        } else {
            "/usr/bin"
        };
        let c: AgentsConfig = toml::from_str(&format!(
            "enabled = true\n[default_limits]\nmax_changes = 3\n[sandbox]\nallow = [[\"cargo\", \"test\"]]\npath_dirs = ['{dir}']\nwrapper = [\"bwrap\", \"--\"]"
        ))
        .unwrap();
        assert!(c.enabled);
        assert_eq!(c.default_limits.max_changes, 3);
        assert_eq!(
            c.default_limits.max_tokens, 1_000_000,
            "unset keys keep their default"
        );
        assert_eq!(c.sandbox.allow, [["cargo", "test"]]);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn bad_bounds_are_refused() {
        for mutate in [
            |c: &mut AgentsConfig| c.max_concurrent_runs = 0,
            |c: &mut AgentsConfig| c.max_concurrent_per_repo = 9,
            |c: &mut AgentsConfig| c.max_task_bytes = 0,
            |c: &mut AgentsConfig| c.max_steps = 0,
            |c: &mut AgentsConfig| c.max_token_ttl_secs = 0,
            |c: &mut AgentsConfig| c.max_file_bytes = c.max_work_bytes + 1,
            |c: &mut AgentsConfig| c.sandbox.allow = vec![vec![]],
            |c: &mut AgentsConfig| c.sandbox.path_dirs = vec!["relative".into()],
            |c: &mut AgentsConfig| c.sandbox.allow = vec![vec!["ls".into()]],
        ] {
            let mut c = AgentsConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err());
        }
    }
}
