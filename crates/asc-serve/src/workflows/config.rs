//! `[workflows]` of `asc-serve.toml`.

use std::path::PathBuf;

use serde::Deserialize;

use crate::agents::config::SandboxConfig;

/// `[workflows]`: repository workflows (docs/WORKFLOWS.md). Off unless `enabled` is set.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkflowsConfig {
    /// The instance runs workflows. Off, no definition is read, no event is looked at and no run
    /// starts; `Workflows.Trigger` answers `WORKFLOW_DISABLED`.
    pub enabled: bool,
    /// Runs one repository may start in 24 hours, whatever started them.
    pub max_runs_per_repo_per_day: u32,
    /// The shortest interval a `schedule` trigger may have, in seconds. A definition that asks
    /// for less is invalid.
    pub min_cron_interval_secs: u32,
    /// Runs this server drives at once.
    pub max_concurrent_runs: u32,
    /// Runs of one workflow waiting for a slot; an event past this is dropped, a manual
    /// trigger is refused.
    pub max_queued_per_workflow: u32,
    /// Seconds between looks at the event sources of a repository.
    pub poll_interval_secs: u32,
    /// Runs one repository's events may start in one look.
    pub max_events_per_poll: u32,
    /// How many workflow runs deep an agent's output may trigger further workflows. A change an
    /// agent made for a workflow run starts a workflow at depth + 1, and none past this.
    pub max_chain_depth: u32,
    /// Bytes of a command step's output kept in the run.
    pub max_output_bytes: usize,
    /// Days finished runs are kept.
    pub run_retention_days: u32,
    /// Where command steps put their working copies. Default: the temporary directory.
    pub work_dir: Option<PathBuf>,
    /// Bounds of a working copy.
    pub max_work_files: usize,
    pub max_work_bytes: u64,
    pub max_file_bytes: u64,
    /// `[workflows.sandbox]`: what command steps may run. Empty: no command step can run, and a
    /// definition that has one says so when it is triggered.
    pub sandbox: SandboxConfig,
}

impl Default for WorkflowsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_runs_per_repo_per_day: 200,
            min_cron_interval_secs: 900,
            max_concurrent_runs: 8,
            max_queued_per_workflow: 20,
            poll_interval_secs: 5,
            max_events_per_poll: 50,
            max_chain_depth: 3,
            max_output_bytes: 32 * 1024,
            run_retention_days: 90,
            work_dir: None,
            max_work_files: 20_000,
            max_work_bytes: 512 * 1024 * 1024,
            max_file_bytes: 64 * 1024 * 1024,
            sandbox: SandboxConfig::default(),
        }
    }
}

impl WorkflowsConfig {
    /// Checks the bounds; the message names the key.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |m: &str| Err(m.to_owned());
        if self.max_runs_per_repo_per_day == 0 || self.max_runs_per_repo_per_day > 100_000 {
            return bad("workflows.max_runs_per_repo_per_day must be 1 to 100000");
        }
        if self.min_cron_interval_secs < 60 || self.min_cron_interval_secs > 86_400 {
            return bad("workflows.min_cron_interval_secs must be 60 to 86400");
        }
        if self.max_concurrent_runs == 0 || self.max_concurrent_runs > 256 {
            return bad("workflows.max_concurrent_runs must be 1 to 256");
        }
        if self.max_queued_per_workflow == 0 || self.max_queued_per_workflow > 1000 {
            return bad("workflows.max_queued_per_workflow must be 1 to 1000");
        }
        if self.poll_interval_secs == 0 || self.poll_interval_secs > 3600 {
            return bad("workflows.poll_interval_secs must be 1 to 3600");
        }
        if self.max_events_per_poll == 0 || self.max_events_per_poll > 1000 {
            return bad("workflows.max_events_per_poll must be 1 to 1000");
        }
        if self.max_chain_depth > 16 {
            return bad("workflows.max_chain_depth must be 0 to 16");
        }
        if self.max_output_bytes < 1024 || self.max_output_bytes > 1024 * 1024 {
            return bad("workflows.max_output_bytes must be 1024 to 1048576");
        }
        if self.run_retention_days == 0 || self.run_retention_days > 3650 {
            return bad("workflows.run_retention_days must be 1 to 3650");
        }
        if self.max_work_files == 0 || self.max_work_bytes == 0 || self.max_file_bytes == 0 {
            return bad(
                "workflows.max_work_files, max_work_bytes and max_file_bytes must be positive",
            );
        }
        if self.max_file_bytes > self.max_work_bytes {
            return bad("workflows.max_file_bytes is above max_work_bytes");
        }
        let s = &self.sandbox;
        if s.allow
            .iter()
            .any(|a| a.is_empty() || a.iter().any(|x| x.is_empty() || x.contains('\0')))
        {
            return bad("workflows.sandbox.allow entries are non-empty argument lists");
        }
        if s.path_dirs.iter().any(|d| !d.is_absolute()) {
            return bad("workflows.sandbox.path_dirs must be absolute");
        }
        if s.max_output_bytes == 0 || s.max_command_seconds == 0 {
            return bad(
                "workflows.sandbox.max_output_bytes and max_command_seconds must be positive",
            );
        }
        if !s.allow.is_empty() && s.path_dirs.is_empty() {
            return bad("workflows.sandbox.allow needs workflows.sandbox.path_dirs");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_off() {
        let c = WorkflowsConfig::default();
        assert!(!c.enabled);
        assert!(c.validate().is_ok());
        assert_eq!(
            (c.max_runs_per_repo_per_day, c.min_cron_interval_secs),
            (200, 900)
        );
    }

    #[test]
    fn parsing_is_strict_and_unset_keys_keep_their_defaults() {
        assert!(toml::from_str::<WorkflowsConfig>("bogus = 1").is_err());
        assert!(toml::from_str::<WorkflowsConfig>("[sandbox]\nbogus = 1").is_err());
        let c: WorkflowsConfig = toml::from_str("enabled = true\nmax_chain_depth = 1").unwrap();
        assert!(c.enabled);
        assert_eq!(c.max_chain_depth, 1);
        assert_eq!(c.max_concurrent_runs, 8);
    }

    #[test]
    fn bad_bounds_are_refused() {
        for mutate in [
            |c: &mut WorkflowsConfig| c.max_runs_per_repo_per_day = 0,
            |c: &mut WorkflowsConfig| c.min_cron_interval_secs = 30,
            |c: &mut WorkflowsConfig| c.max_concurrent_runs = 0,
            |c: &mut WorkflowsConfig| c.max_queued_per_workflow = 0,
            |c: &mut WorkflowsConfig| c.poll_interval_secs = 0,
            |c: &mut WorkflowsConfig| c.max_events_per_poll = 0,
            |c: &mut WorkflowsConfig| c.max_chain_depth = 17,
            |c: &mut WorkflowsConfig| c.max_output_bytes = 10,
            |c: &mut WorkflowsConfig| c.run_retention_days = 0,
            |c: &mut WorkflowsConfig| c.max_file_bytes = c.max_work_bytes + 1,
            |c: &mut WorkflowsConfig| c.sandbox.allow = vec![vec![]],
            |c: &mut WorkflowsConfig| c.sandbox.path_dirs = vec!["relative".into()],
            |c: &mut WorkflowsConfig| c.sandbox.allow = vec![vec!["ls".into()]],
        ] {
            let mut c = WorkflowsConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err());
        }
    }
}
