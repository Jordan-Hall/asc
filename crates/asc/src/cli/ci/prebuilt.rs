//! `asc ci prebuilt register|check` (SPEC §12.4, §15.5.1 build level): the client side of the
//! `build` path level. CI registers each turbo task's output under the directory it was built
//! from, so the remote cache serves it to people who may build that directory but not read it;
//! `check` tells such a person, before a build fails on missing source, which restricted
//! directories have no prebuilt output for the current inputs.
//!
//! Both read the run summary turbo writes with `--summarize` (`.turbo/runs/<id>.json`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::pathacl::Level;
use clap::{Parser, Subcommand};
use serde::Deserialize;

use super::cache::{connect, fatal, root_of};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::path_lock;

/// Every flag of `ci prebuilt`, for the native table.
pub const FLAGS: &[&str] = &["--root", "--remote", "--summary"];

/// Most tasks sent in one registration request (the server's JSON body limit).
const BATCH: usize = 500;

#[derive(Parser, Debug)]
#[command(
    name = "ci prebuilt",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Registers each task's output under its directory (trusted CI only).
    Register(Common),
    /// Exits 1 when a restricted directory has no prebuilt output for its inputs.
    Check(Common),
}

#[derive(clap::Args, Debug)]
struct Common {
    /// The work tree root (default: the current directory).
    #[arg(long)]
    root: Option<PathBuf>,
    /// A configured remote or a server URL (default `origin`).
    #[arg(long, env = "ASC_REMOTE")]
    remote: Option<String>,
    /// The run summary (default: the newest in `<root>/.turbo/runs`).
    #[arg(long)]
    summary: Option<PathBuf>,
}

#[derive(Deserialize, Debug)]
struct Summary {
    #[serde(default)]
    tasks: Vec<Task>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct Task {
    #[serde(rename = "taskId", default)]
    pub id: String,
    pub hash: String,
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub cache: CacheState,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CacheState {
    #[serde(default)]
    pub status: String,
}

impl Task {
    /// The directory as a repository path: `/`-separated, no `.` or `./`.
    fn dir(&self) -> String {
        let d = self.directory.replace('\\', "/");
        let d = d.trim_start_matches("./").trim_end_matches('/');
        if d == "." {
            String::new()
        } else {
            d.to_owned()
        }
    }

    fn hit(&self) -> bool {
        self.cache.status.eq_ignore_ascii_case("HIT")
    }
}

/// Parses a run summary, keeping tasks with a hash.
pub(super) fn parse(text: &str) -> Result<Vec<Task>> {
    let s: Summary = serde_json::from_str(text)
        .map_err(|e| CliError::Fatal(format!("asc: invalid turbo run summary: {e}")))?;
    Ok(s.tasks.into_iter().filter(|t| !t.hash.is_empty()).collect())
}

/// The newest `*.json` in `<root>/.turbo/runs`.
fn newest(root: &Path) -> Result<PathBuf> {
    let dir = root.join(".turbo").join("runs");
    let none = || {
        CliError::Fatal(format!(
            "asc: no turbo run summary in {}; run turbo with --summarize or pass --summary",
            dir.display()
        ))
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(&dir).map_err(|_| none())?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json")
            && let Ok(m) = e.metadata().and_then(|m| m.modified())
            && best.as_ref().is_none_or(|(t, _)| m > *t)
        {
            best = Some((m, p));
        }
    }
    best.map(|(_, p)| p).ok_or_else(none)
}

fn load(ctx: &Ctx, c: &Common) -> Result<Vec<Task>> {
    let root = root_of(ctx, c.root.as_deref())?;
    let path = match &c.summary {
        Some(p) => ctx.cwd.join(p),
        None => newest(&root)?,
    };
    parse(&std::fs::read_to_string(&path)?)
}

/// The tasks that would fail for this person: built from a directory they may not read and
/// not served from the cache.
pub(super) fn unbuilt<'t>(tasks: &'t [Task], level: impl Fn(&str) -> Level) -> Vec<&'t Task> {
    tasks
        .iter()
        .filter(|t| level(&t.dir()) < Level::Read && !t.hit())
        .collect()
}

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a = Args::try_parse_from(
        std::iter::once(OsString::from("ci prebuilt")).chain(rest.iter().cloned()),
    )
    .map_err(|e| CliError::Usage(e.to_string()))?;
    match a.cmd {
        Cmd::Register(c) => {
            let tasks = load(ctx, &c)?;
            let conn = connect(ctx, c.remote.as_deref()).await?;
            let (mut registered, mut missing) = (0u64, 0usize);
            for chunk in tasks.chunks(BATCH) {
                let body = serde_json::json!({
                    "artifacts": chunk
                        .iter()
                        .map(|t| serde_json::json!({ "hash": t.hash, "path": t.dir() }))
                        .collect::<Vec<_>>()
                });
                let r = conn
                    .api
                    .post_turbo("/v8/asc/artifact-paths", &body)
                    .await
                    .map_err(fatal)?;
                registered += r["registered"].as_u64().unwrap_or(0);
                missing += r["missing"].as_array().map_or(0, Vec::len);
            }
            eprintln!("registered {registered} prebuilt outputs ({missing} not in the cache)");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Check(c) => {
            let tasks = load(ctx, &c)?;
            let access = ctx.ws().ok().and_then(path_lock::load);
            let Some(access) = access else {
                return Ok(ExitCode::SUCCESS);
            };
            let bad = unbuilt(&tasks, |p| access.level(p));
            for t in &bad {
                let dir = if t.dir().is_empty() { "." } else { &t.dir() };
                eprintln!(
                    "{dir}: restricted (build level: no cached output of {} for {}; run CI)",
                    t.id, t.hash
                );
            }
            Ok(if bad.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = r#"{"id":"r","tasks":[
        {"taskId":"web#build","hash":"aa","directory":"apps/web","cache":{"status":"MISS"}},
        {"taskId":"core#build","hash":"bb","directory":"./secret/core/","cache":{"status":"HIT"}},
        {"taskId":"gen#build","hash":"cc","directory":"secret\\gen","cache":{"status":"MISS"}},
        {"taskId":"root#lint","hash":"dd","directory":".","cache":{"status":"MISS"}},
        {"taskId":"x#y","hash":"","directory":"secret/x"}
    ]}"#;

    #[test]
    fn the_summary_gives_repository_paths() {
        let t = parse(SUMMARY).unwrap();
        assert_eq!(t.len(), 4);
        let dirs: Vec<_> = t.iter().map(Task::dir).collect();
        assert_eq!(dirs, ["apps/web", "secret/core", "secret/gen", ""]);
        assert!(parse("not json").is_err());
    }

    #[test]
    fn only_a_restricted_miss_is_unbuilt() {
        let t = parse(SUMMARY).unwrap();
        let level = |p: &str| {
            if p.starts_with("secret") {
                Level::Build
            } else {
                Level::Write
            }
        };
        let bad: Vec<_> = unbuilt(&t, level).into_iter().map(|t| &t.hash).collect();
        assert_eq!(bad, ["cc"]);
    }

    #[test]
    fn the_newest_summary_is_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(newest(dir.path()).is_err());
        let runs = dir.path().join(".turbo/runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(runs.join("a.json"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(runs.join("b.json"), "{}").unwrap();
        std::fs::write(runs.join("c.txt"), "").unwrap();
        assert_eq!(newest(dir.path()).unwrap(), runs.join("b.json"));
    }
}
