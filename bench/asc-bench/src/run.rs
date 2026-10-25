//! Scenario execution: builds one repository per tool and workload, then times
//! each scenario with the same arguments on git and on asc.
//!
//! Every spawn is non-interactive, ignores the user's and the system's git
//! configuration, passes `-c init.defaultBranch=master`, has a timeout and
//! captures at most `STDERR_CAP` bytes of stderr. The asc binary is never found
//! on `PATH`.

use crate::synth::{self, Config};
use crate::{Class, Disk, Outcome, Row, stat};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(600);
const STDERR_CAP: usize = 8 * 1024;
/// Commits after the initial one in every generated history.
const HISTORY: u64 = 6;

/// One tool (git or asc) with a clean, isolated environment.
pub struct Tool {
    pub name: &'static str,
    pub exe: PathBuf,
    empty_config: PathBuf,
}

/// Why a command did not succeed.
#[derive(Debug)]
pub enum Fail {
    Unsupported(String),
    Error(String),
}

impl Fail {
    fn outcome(self) -> Outcome {
        match self {
            Fail::Unsupported(reason) => Outcome::Unsupported { reason },
            Fail::Error(reason) => Outcome::Error { reason },
        }
    }
}

/// Classify asc stderr: a command asc does not implement yet is a result, not a harness bug.
pub fn classify(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "not supported yet",
        "not implemented",
        "unrecognized subcommand",
        "unknown command",
        "unsupported",
    ]
    .iter()
    .any(|m| s.contains(m))
}

impl Tool {
    pub fn new(name: &'static str, exe: PathBuf, home: &Path) -> std::io::Result<Self> {
        let empty_config = home.join(format!("{name}.empty.gitconfig"));
        fs::write(&empty_config, "")?;
        Ok(Tool {
            name,
            exe,
            empty_config,
        })
    }

    /// The isolated, non-interactive command line for `args` in `cwd` (stdio not set).
    pub fn command(&self, cwd: &Path, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.args([
            "-c",
            "init.defaultBranch=master",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &self.empty_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "bench")
        .env("GIT_AUTHOR_EMAIL", "bench@example.invalid")
        .env("GIT_COMMITTER_NAME", "bench")
        .env("GIT_COMMITTER_EMAIL", "bench@example.invalid")
        .stdin(Stdio::null());
        cmd
    }

    /// Run `args` in `cwd`; returns wall time and bounded stderr text on success.
    pub fn run(&self, cwd: &Path, args: &[String]) -> Result<Duration, Fail> {
        let mut cmd = self.command(cwd, args);
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| Fail::Error(format!("spawn {}: {e}", self.exe.display())))?;
        let mut err = child.stderr.take().expect("stderr piped");
        let reader = std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if kept.len() < STDERR_CAP {
                    kept.extend_from_slice(&buf[..n.min(STDERR_CAP - kept.len())]);
                }
            }
            String::from_utf8_lossy(&kept).into_owned()
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if start.elapsed() > TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Fail::Error(format!(
                        "{} {} timed out",
                        self.name,
                        args.join(" ")
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => return Err(Fail::Error(e.to_string())),
            }
        };
        let elapsed = start.elapsed();
        let stderr = reader.join().unwrap_or_default();
        if status.success() {
            Ok(elapsed)
        } else if self.name == "asc" && classify(&stderr) {
            Err(Fail::Unsupported(bounded(&stderr)))
        } else {
            Err(Fail::Error(format!(
                "{} {} failed ({}): {}",
                self.name,
                args.join(" "),
                status,
                bounded(&stderr)
            )))
        }
    }

    pub fn version(&self, cwd: &Path) -> String {
        let out = Command::new(&self.exe)
            .arg("--version")
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .trim()
                .chars()
                .take(120)
                .collect(),
            Err(_) => "unknown".into(),
        }
    }
}

fn bounded(s: &str) -> String {
    s.trim().chars().take(300).collect()
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}

/// Create the workload repository with `HISTORY + 1` commits and a `side` branch three commits back.
pub fn build_repo(tool: &Tool, dir: &Path, cfg: &Config) -> Result<(), Fail> {
    let io = |e: std::io::Error| Fail::Error(e.to_string());
    fs::create_dir_all(dir).map_err(io)?;
    tool.run(dir, &args(&["init", "-q"]))?;
    synth::generate(cfg, dir).map_err(io)?;
    tool.run(dir, &args(&["add", "-A"]))?;
    tool.run(dir, &args(&["commit", "-q", "-m", "c0"]))?;
    for k in 1..=HISTORY {
        synth::churn(cfg, dir, k).map_err(io)?;
        tool.run(dir, &args(&["add", "-A"]))?;
        tool.run(dir, &args(&["commit", "-q", "-m", &format!("c{k}")]))?;
    }
    tool.run(dir, &args(&["branch", "side", "HEAD~3"]))?;
    Ok(())
}

/// What happens before the timed command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prep {
    None,
    /// Once, before the first run: leave modified files in the tree.
    Dirty,
    /// Before every run: modify a file and stage it (untimed).
    Staged,
}

struct Ctx<'a> {
    repo: &'a Path,
    scratch: &'a Path,
    cfg: &'a Config,
    tag: &'a str,
    run: usize,
}

struct Scenario {
    name: &'static str,
    class: Class,
    prep: Prep,
    /// Directories created by the command, removed before each run and measured after the last.
    creates: bool,
    args: fn(&Ctx) -> Vec<String>,
}

fn out_path(c: &Ctx, what: &str) -> String {
    c.scratch
        .join(format!("{what}-{}-{}", c.tag, c.run))
        .to_string_lossy()
        .into_owned()
}

fn scenarios() -> Vec<Scenario> {
    use Class::{Elsewhere, Scion};
    vec![
        Scenario {
            name: "status-clean",
            class: Elsewhere,
            prep: Prep::None,
            creates: false,
            args: |_| args(&["status", "--porcelain"]),
        },
        Scenario {
            name: "log-path",
            class: Scion,
            prep: Prep::None,
            creates: false,
            args: |_| args(&["log", "--oneline", "--", "d0_0"]),
        },
        Scenario {
            name: "merge-base",
            class: Scion,
            prep: Prep::None,
            creates: false,
            args: |_| args(&["merge-base", "master", "side"]),
        },
        Scenario {
            name: "blame",
            class: Scion,
            prep: Prep::None,
            creates: false,
            args: |c| {
                vec![
                    "blame".into(),
                    "--".into(),
                    synth::path_of(c.cfg, 0)
                        .to_string_lossy()
                        .replace('\\', "/"),
                ]
            },
        },
        Scenario {
            name: "diff-stat",
            class: Elsewhere,
            prep: Prep::None,
            creates: false,
            args: |_| args(&["diff", "--stat", "side", "master"]),
        },
        Scenario {
            name: "switch",
            class: Scion,
            prep: Prep::None,
            creates: false,
            args: |c| {
                args(&[
                    "switch",
                    "-q",
                    if c.run % 2 == 0 { "side" } else { "master" },
                ])
            },
        },
        Scenario {
            name: "clone-local",
            class: Scion,
            prep: Prep::None,
            creates: true,
            args: |c| {
                vec![
                    "clone".into(),
                    "-q".into(),
                    c.repo.to_string_lossy().into_owned(),
                    out_path(c, "clone"),
                ]
            },
        },
        Scenario {
            name: "worktree-add",
            class: Scion,
            prep: Prep::None,
            creates: true,
            args: |c| {
                vec![
                    "worktree".into(),
                    "add".into(),
                    "-q".into(),
                    "-b".into(),
                    format!("wt-{}", c.run),
                    out_path(c, "wt"),
                ]
            },
        },
        Scenario {
            name: "status-dirty",
            class: Elsewhere,
            prep: Prep::Dirty,
            creates: false,
            args: |_| args(&["status", "--porcelain"]),
        },
        Scenario {
            name: "diff-head",
            class: Elsewhere,
            prep: Prep::None,
            creates: false,
            args: |_| args(&["diff", "HEAD"]),
        },
        Scenario {
            name: "commit",
            class: Elsewhere,
            prep: Prep::Staged,
            creates: false,
            args: |c| args(&["commit", "-q", "-m", &format!("bench {}", c.run)]),
        },
    ]
}

/// Names of every scenario, for `--list` and tests.
pub fn scenario_names() -> Vec<&'static str> {
    scenarios().iter().map(|s| s.name).collect()
}

/// Total size of regular files under `p`, not following symlinks.
pub fn dir_bytes(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

struct Measured {
    outcome: Outcome,
    disk: Option<u64>,
}

fn measure(
    tool: &Tool,
    sc: &Scenario,
    repo: &Path,
    scratch: &Path,
    cfg: &Config,
    runs: usize,
) -> Measured {
    let mut ms = Vec::new();
    let mut disk = None;
    // run 0 is an untimed warm-up
    for run in 0..=runs {
        let ctx = Ctx {
            repo,
            scratch,
            cfg,
            tag: tool.name,
            run,
        };
        let prepared = (|| -> Result<(), Fail> {
            let io = |e: std::io::Error| Fail::Error(e.to_string());
            match sc.prep {
                Prep::Dirty if run == 0 => {
                    synth::churn(cfg, repo, 1000).map_err(io)?;
                }
                Prep::Staged => {
                    synth::churn(cfg, repo, 2000 + run as u64).map_err(io)?;
                    tool.run(repo, &args(&["add", "-A"]))?;
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(f) = prepared {
            return Measured {
                outcome: f.outcome(),
                disk,
            };
        }
        match tool.run(repo, &(sc.args)(&ctx)) {
            Ok(d) if run > 0 => ms.push(d.as_secs_f64() * 1000.0),
            Ok(_) => {}
            Err(f) => {
                return Measured {
                    outcome: f.outcome(),
                    disk,
                };
            }
        }
        if sc.creates {
            let made = PathBuf::from(out_path(
                &ctx,
                if sc.name == "clone-local" {
                    "clone"
                } else {
                    "wt"
                },
            ));
            if run == runs {
                disk = Some(dir_bytes(&made));
            }
            let _ = fs::remove_dir_all(&made);
        }
    }
    match stat(&ms) {
        Some(s) => Measured {
            outcome: Outcome::Ok(s),
            disk,
        },
        None => Measured {
            outcome: Outcome::Error {
                reason: "no timed runs".into(),
            },
            disk,
        },
    }
}

/// A named workload.
pub struct Workload {
    pub name: &'static str,
    pub cfg: Config,
    pub runs: usize,
}

pub fn suite(name: &str) -> Option<Vec<Workload>> {
    let small = Workload {
        name: "small",
        cfg: Config::small(),
        runs: 5,
    };
    let ten = Workload {
        name: "10x",
        cfg: Config::ten_x(),
        runs: 5,
    };
    match name {
        "quick" => Some(vec![small, ten]),
        "full" => Some(vec![
            small,
            ten,
            Workload {
                name: "100x",
                cfg: Config {
                    files: 50_000,
                    depth: 5,
                    fanout: 8,
                    seed: 3,
                    ..Config::small()
                },
                runs: 3,
            },
        ]),
        _ => None,
    }
}

/// Run every scenario whose name contains `only` (all when `None`) on `workloads`.
pub fn run_suite(
    git: &Tool,
    asc: Option<&Tool>,
    workloads: &[Workload],
    only: Option<&str>,
    work: &Path,
) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for w in workloads {
        let scs: Vec<_> = scenarios()
            .into_iter()
            .filter(|s| only.is_none_or(|o| s.name.contains(o)))
            .collect();
        if scs.is_empty() {
            continue;
        }
        let wdir = work.join(w.name);
        let scratch = wdir.join("scratch");
        fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        let git_repo = wdir.join("git");
        build_repo(git, &git_repo, &w.cfg).map_err(|f| format!("git setup failed: {f:?}"))?;
        let asc_repo = wdir.join("asc");
        let asc_ready = match asc {
            Some(c) => build_repo(c, &asc_repo, &w.cfg).map_err(|f| match f {
                Fail::Unsupported(r) | Fail::Error(r) => r,
            }),
            None => Err("asc binary not given (set ASC_BIN or --asc)".to_string()),
        };
        for sc in &scs {
            eprintln!("{}/{} ...", w.name, sc.name);
            let g = measure(git, sc, &git_repo, &scratch, &w.cfg, w.runs);
            let c = match (&asc_ready, asc) {
                (Ok(()), Some(c)) => measure(c, sc, &asc_repo, &scratch, &w.cfg, w.runs),
                (Err(reason), _) => Measured {
                    outcome: Outcome::Unsupported {
                        reason: format!("asc repo setup failed: {reason}"),
                    },
                    disk: None,
                },
                _ => unreachable!("asc_ready is Ok only with an asc tool"),
            };
            rows.push(Row {
                workload: w.name.into(),
                scenario: sc.name.into(),
                class: sc.class,
                git: g.outcome,
                asc: c.outcome,
                disk: Disk {
                    git: g.disk,
                    asc: c.disk,
                },
            });
        }
        let _ = fs::remove_dir_all(&wdir);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_separates_missing_commands_from_errors() {
        assert!(classify("error: blame is not supported yet"));
        assert!(classify("error: unrecognized subcommand 'worktree'"));
        assert!(!classify("fatal: pathspec 'x' did not match"));
    }

    #[test]
    fn scenario_set_matches_spec_gate_classes() {
        let names = scenario_names();
        for n in [
            "clone-local",
            "worktree-add",
            "switch",
            "merge-base",
            "blame",
            "log-path",
        ] {
            assert!(names.contains(&n));
        }
        for s in scenarios() {
            let scion = [
                "clone-local",
                "worktree-add",
                "switch",
                "merge-base",
                "blame",
                "log-path",
            ]
            .contains(&s.name);
            assert_eq!(s.class == Class::Scion, scion, "{}", s.name);
        }
    }

    #[test]
    fn suites_exist() {
        assert_eq!(suite("quick").unwrap().len(), 2);
        assert!(suite("nope").is_none());
    }

    #[test]
    fn dir_bytes_sums_files() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("a")).unwrap();
        fs::write(d.path().join("a/x"), [0u8; 10]).unwrap();
        fs::write(d.path().join("y"), [0u8; 5]).unwrap();
        assert_eq!(dir_bytes(d.path()), 15);
    }

    #[test]
    fn missing_asc_yields_unsupported_rows() {
        // git is required for the real run; here only check that an absent asc is reported, not fatal.
        let home = tempfile::tempdir().unwrap();
        let git = Tool::new("git", PathBuf::from("git"), home.path()).unwrap();
        if git.version(home.path()) == "unknown" {
            return;
        }
        let w = [Workload {
            name: "t",
            cfg: Config {
                files: 12,
                depth: 1,
                fanout: 3,
                median_bytes: 128,
                churn_files: 2,
                seed: 4,
            },
            runs: 1,
        }];
        let rows = run_suite(&git, None, &w, Some("merge-base"), home.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].git, Outcome::Ok(_)), "{:?}", rows[0].git);
        assert!(matches!(rows[0].asc, Outcome::Unsupported { .. }));
    }
}
