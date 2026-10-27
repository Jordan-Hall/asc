//! Reading workflow definitions from the repository.
//!
//! The definitions in force are the files `.asc/workflows/<name>.toml` of the trunk tip, never
//! the files of a change under review, so a change cannot give itself a workflow. A file that
//! does not parse is kept as an invalid definition with its error, so a person sees why it does
//! not run; it never runs. The server adds one rule on top of the file format: a schedule may not
//! fire more often than `[workflows] min_cron_interval_secs`.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::workflow::{MAX_WORKFLOW_BYTES, TriggerKind, WORKFLOW_DIR, Workflow, valid_name};
use tonic::Status;

use super::config::WorkflowsConfig;
use crate::agents::repo::RepoView;
use crate::registry::RepoEntry;

/// Definitions one repository may carry.
pub const MAX_DEFINITIONS: usize = 200;

/// One file of `.asc/workflows/`.
#[derive(Debug)]
pub struct Def {
    /// The file stem.
    pub name: String,
    /// `.asc/workflows/<name>.toml`.
    pub path: String,
    /// The bytes of the file (empty when it was too large to read).
    pub text: Vec<u8>,
    pub parsed: Result<Workflow, DefError>,
}

/// Why a definition cannot run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefError {
    pub message: String,
    pub line: u32,
    pub column: u32,
}

impl Def {
    pub fn workflow(&self) -> Option<&Workflow> {
        self.parsed.as_ref().ok()
    }

    pub fn error(&self) -> String {
        match &self.parsed {
            Ok(_) => String::new(),
            Err(e) if e.line > 0 => format!("{}:{}: {}", e.line, e.column, e.message),
            Err(e) => e.message.clone(),
        }
    }
}

/// The definitions of a trunk tip.
#[derive(Debug, Default)]
pub struct Defs {
    /// The trunk changeset they were read from.
    pub source: Option<ChangesetId>,
    pub list: Vec<Arc<Def>>,
}

impl Defs {
    pub fn get(&self, name: &str) -> Option<&Arc<Def>> {
        self.list.iter().find(|d| d.name == name)
    }
}

/// Parses one file and applies the server's own rules.
pub fn parse(name: &str, text: &[u8], cfg: &WorkflowsConfig) -> Result<Workflow, DefError> {
    let wf = Workflow::parse(name, text).map_err(|e| DefError {
        message: e.message,
        line: e.line,
        column: e.column,
    })?;
    for t in &wf.triggers {
        if t.kind != TriggerKind::Schedule {
            continue;
        }
        let Some(cron) = &t.cron else { continue };
        // A schedule that matches at most once a year has no interval to be too short.
        if let Some(gap) = cron.min_interval_secs()
            && gap < u64::from(cfg.min_cron_interval_secs)
        {
            return Err(DefError {
                message: format!(
                    "the schedule {:?} fires every {gap} seconds; this server allows one every {} seconds at most",
                    cron.as_str(),
                    cfg.min_cron_interval_secs
                ),
                line: 0,
                column: 0,
            });
        }
    }
    Ok(wf)
}

/// Reads every definition of the trunk tip of `entry`.
pub async fn load(
    view: &RepoView,
    entry: &RepoEntry,
    cfg: &WorkflowsConfig,
) -> Result<Defs, Status> {
    let Some(tip) = view.trunk_tip(entry).await? else {
        return Ok(Defs::default());
    };
    load_at(view, entry, tip, cfg).await
}

/// Reads every definition of changeset `cs`.
pub async fn load_at(
    view: &RepoView,
    entry: &RepoEntry,
    cs: ChangesetId,
    cfg: &WorkflowsConfig,
) -> Result<Defs, Status> {
    let root = view.root_of(entry, cs).await?;
    let Some(entries) = view.list_dir(entry, root, WORKFLOW_DIR).await? else {
        return Ok(Defs {
            source: Some(cs),
            list: Vec::new(),
        });
    };
    let mut files: Vec<_> = entries
        .into_iter()
        .filter(|e| !e.dir && e.name.ends_with(".toml"))
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files.truncate(MAX_DEFINITIONS);
    let mut list = Vec::with_capacity(files.len());
    for f in files {
        let stem = f.name.trim_end_matches(".toml").to_owned();
        let path = format!("{WORKFLOW_DIR}/{}", f.name);
        if !valid_name(&stem) {
            list.push(Arc::new(Def {
                name: stem,
                path,
                text: Vec::new(),
                parsed: Err(DefError {
                    message: "a workflow file is named <name>.toml with a name of [a-z0-9_-]"
                        .into(),
                    line: 0,
                    column: 0,
                }),
            }));
            continue;
        }
        if f.size > MAX_WORKFLOW_BYTES as u64 {
            list.push(Arc::new(Def {
                name: stem,
                path,
                text: Vec::new(),
                parsed: Err(DefError {
                    message: format!("the workflow file is larger than {MAX_WORKFLOW_BYTES} bytes"),
                    line: 0,
                    column: 0,
                }),
            }));
            continue;
        }
        let text = view
            .read(entry, root, &path, MAX_WORKFLOW_BYTES as u64)
            .await?
            .map(|b| b.to_vec())
            .unwrap_or_default();
        let parsed = parse(&stem, &text, cfg);
        list.push(Arc::new(Def {
            name: stem,
            path,
            text,
            parsed,
        }));
    }
    Ok(Defs {
        source: Some(cs),
        list,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
[[step]]
id = "a"
kind = "approval"
approvers = ["role:maintainer"]
"#;

    #[test]
    fn a_schedule_cannot_fire_more_often_than_the_server_allows() {
        let cfg = WorkflowsConfig::default();
        let with =
            |cron: &str| format!("[[trigger]]\nkind = \"schedule\"\ncron = \"{cron}\"\n{OK}");
        assert!(parse("w", with("0 3 * * *").as_bytes(), &cfg).is_ok());
        assert!(
            parse("w", with("*/15 * * * *").as_bytes(), &cfg).is_ok(),
            "exactly the limit"
        );
        let e = parse("w", with("*/5 * * * *").as_bytes(), &cfg).unwrap_err();
        assert!(e.message.contains("300 seconds"), "{}", e.message);
        let e = parse("w", with("* * * * *").as_bytes(), &cfg).unwrap_err();
        assert!(e.message.contains("60 seconds"));
        let loose = WorkflowsConfig {
            min_cron_interval_secs: 60,
            ..WorkflowsConfig::default()
        };
        assert!(parse("w", with("* * * * *").as_bytes(), &loose).is_ok());
        // A syntax error keeps its place in the file.
        let e = parse("w", b"[[step]\nid = 1", &cfg).unwrap_err();
        assert!(e.line >= 1);
    }
}
