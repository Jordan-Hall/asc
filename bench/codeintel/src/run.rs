//! Runs the query scenarios: builds the deterministic repository, spawns each tool with a fixed argv
//! (no shell, patterns after `-e` or `--` so a pattern is never an option) and times it.

use crate::{Outcome, Query, QueryKind, Row, Tool, Workload, stat};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Explicit tool paths. A tool left `None` is recorded as unsupported; none is searched on PATH
/// except `git`, as in O05.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    pub git: PathBuf,
    pub asc: Option<PathBuf>,
    pub rg: Option<PathBuf>,
    pub zoekt: Option<PathBuf>,
    pub zoekt_git_index: Option<PathBuf>,
}

/// Files per synthetic workload. `--medium-repo` and `--huge-repo` replace a workload with a real repo
/// (the linux import, a chromium snapshot).
pub fn files_for(w: Workload) -> usize {
    match w {
        Workload::Small => 200,
        Workload::Medium => 4_000,
        Workload::Huge => 40_000,
    }
}

pub fn suite(name: &str) -> Option<Vec<Workload>> {
    match name {
        "codeintel-quick" => Some(vec![Workload::Small, Workload::Medium]),
        "codeintel-full" => Some(vec![Workload::Small, Workload::Medium, Workload::Huge]),
        _ => None,
    }
}

/// Deterministic file content: planted rare marker (1 file in 1000), regex target (1 in 50),
/// word target (1 in 10), and `for` everywhere.
pub fn file_text(i: usize) -> String {
    let mut s = String::new();
    let mut x = (i as u64)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    for line in 0..40 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let n = (x >> 33) % 997;
        s.push_str(&format!(
            "fn item_{i}_{line}(v: u32) -> u32 {{ for k in 0..{n} {{ sink(v + k); }} v }}\n"
        ));
    }
    if i % 1000 == 7 {
        s.push_str("// zqrare_marker_7731\n");
    }
    if i % 50 == 3 {
        s.push_str("let fooAlphaBar = 1;\n");
    }
    if i % 10 == 1 {
        s.push_str("fn handle_request() {}\n");
    }
    s
}

fn git(git: &Path, dir: &Path, args: &[&str]) -> Result<(), String> {
    let o = Command::new(git)
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=bench",
            "-c",
            "user.email=bench@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

/// Builds the synthetic repository under `dir` (branch `master`, one commit).
pub fn build_repo(git_bin: &Path, dir: &Path, files: usize) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(git_bin, dir, &["init", "-q", "-b", "master"])?;
    for i in 0..files {
        let sub = dir.join(format!("d{:03}", i % 100));
        fs::create_dir_all(&sub).map_err(|e| e.to_string())?;
        fs::write(sub.join(format!("f{i}.rs")), file_text(i)).map_err(|e| e.to_string())?;
    }
    git(git_bin, dir, &["add", "-A"])?;
    git(git_bin, dir, &["commit", "-q", "-m", "bench corpus"])
}

fn flags(k: QueryKind) -> &'static [&'static str] {
    match k {
        QueryKind::Literal => &["-F"],
        QueryKind::Regex => &["-E"],
        QueryKind::Icase => &["-F", "-i"],
        QueryKind::Word => &["-F", "-w"],
    }
}

/// The argv (program first) for `tool` on `q`, or `None` when the tool has no such mode.
/// `repo` is the checkout, `idx` the Zoekt index directory.
pub fn argv(tool: Tool, tools: &Tools, q: &Query, repo: &Path, idx: &Path) -> Option<Vec<String>> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut v: Vec<String> = Vec::new();
    match tool {
        Tool::Asc => {
            // TODO(I17): confirm the final `asc search` flag names against the CLI of I17.
            v.push(s(tools.asc.as_deref()?));
            v.extend(["search".into(), "--no-color".into()]);
            v.extend(flags(q.kind).iter().map(|f| f.to_string()));
            v.extend(["--".into(), q.pattern.clone()]);
        }
        Tool::RgWarm | Tool::RgCold => {
            v.push(s(tools.rg.as_deref()?));
            v.extend(["-n".into(), "--no-heading".into(), "--no-config".into()]);
            v.extend(
                flags(q.kind)
                    .iter()
                    .map(|f| match *f {
                        "-E" => String::new(),
                        other => other.to_string(),
                    })
                    .filter(|f| !f.is_empty()),
            );
            v.extend(["-e".into(), q.pattern.clone(), "--".into(), s(repo)]);
        }
        Tool::GitGrepWorktree | Tool::GitGrepRev => {
            v.extend([
                s(&tools.git),
                "-C".into(),
                s(repo),
                "grep".into(),
                "-n".into(),
                "--no-color".into(),
            ]);
            v.extend(flags(q.kind).iter().map(|f| f.to_string()));
            v.extend(["-e".into(), q.pattern.clone()]);
            if tool == Tool::GitGrepRev {
                v.extend(["master".into(), "--".into()]);
            }
        }
        Tool::Zoekt => {
            v.push(s(tools.zoekt.as_deref()?));
            v.extend(["-index_dir".into(), s(idx)]);
            let lit = q.pattern.replace('\\', "\\\\").replace('"', "\\\"");
            v.push(match q.kind {
                QueryKind::Literal => format!("case:yes \"{lit}\""),
                QueryKind::Icase => format!("case:no \"{lit}\""),
                QueryKind::Word => format!("case:yes \\b{}\\b", regex_escape(&q.pattern)),
                QueryKind::Regex => format!("case:yes {}", q.pattern),
            });
        }
    }
    Some(v)
}

fn regex_escape(p: &str) -> String {
    let mut o = String::new();
    for c in p.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

enum Run {
    Ms(f64),
    Unsupported(String),
    Failed(String),
}

fn exec(argv: &[String], cwd: &Path) -> Run {
    let t = Instant::now();
    let o = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    match o {
        Err(e) => Run::Failed(format!("cannot start {}: {e}", argv[0])),
        // rg and git grep exit 1 for "no match", which is a valid answer.
        Ok(o) if matches!(o.status.code(), Some(0 | 1)) => Run::Ms(ms),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().next().unwrap_or("").to_string();
            if err.contains("unrecognized") || err.contains("unexpected argument") {
                Run::Unsupported(first)
            } else {
                Run::Failed(first)
            }
        }
    }
}

fn measure(tool: Tool, args: Option<Vec<String>>, cwd: &Path, runs: usize) -> Outcome {
    let Some(args) = args else {
        return Outcome::Unsupported {
            reason: format!("{} not configured", tool.name()),
        };
    };
    // The first run is the cold sample; `rg-cold` is that run alone, the others take the rest.
    let n = if tool == Tool::RgCold { 1 } else { runs + 1 };
    let mut samples = Vec::new();
    for i in 0..n {
        match exec(&args, cwd) {
            Run::Ms(ms) => {
                if tool == Tool::RgCold || i > 0 {
                    samples.push(ms);
                }
            }
            Run::Unsupported(reason) => return Outcome::Unsupported { reason },
            Run::Failed(reason) => return Outcome::Error { reason },
        }
    }
    match stat(&samples) {
        Some(s) => Outcome::Ok(s),
        None => Outcome::Error {
            reason: "no samples".into(),
        },
    }
}

/// Runs every query on a repo; `repo` must hold branch `master`.
pub fn run_queries(
    tools: &Tools,
    workload: Workload,
    queries: &[Query],
    repo: &Path,
    idx: &Path,
    runs: usize,
) -> Vec<Row> {
    queries
        .iter()
        .map(|q| {
            let mut map = BTreeMap::new();
            for t in Tool::ALL {
                map.insert(t, measure(t, argv(t, tools, q, repo, idx), repo, runs));
            }
            Row {
                workload,
                query: q.name.clone(),
                tools: map,
            }
        })
        .collect()
}

/// Builds the Zoekt index of `repo` into `idx` when `zoekt-git-index` is configured.
pub fn build_zoekt(tools: &Tools, repo: &Path, idx: &Path) -> Result<(), String> {
    let Some(bin) = &tools.zoekt_git_index else {
        return Ok(());
    };
    fs::create_dir_all(idx).map_err(|e| e.to_string())?;
    let o = Command::new(bin)
        .arg("-index")
        .arg(idx)
        .arg(repo)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("zoekt-git-index: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(format!(
            "zoekt-git-index: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

pub fn tool_version(bin: &Path, arg: &str) -> String {
    Command::new(bin)
        .arg(arg)
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Tools {
        Tools {
            git: "git".into(),
            asc: Some("asc".into()),
            rg: Some("rg".into()),
            zoekt: Some("zoekt".into()),
            zoekt_git_index: None,
        }
    }
    fn q(kind: QueryKind, p: &str) -> Query {
        Query {
            name: "n".into(),
            kind,
            pattern: p.into(),
        }
    }

    #[test]
    fn corpus_is_deterministic_and_plants_targets() {
        assert_eq!(file_text(7), file_text(7));
        assert!(file_text(7).contains("zqrare_marker_7731"));
        assert!(!file_text(8).contains("zqrare_marker_7731"));
        assert!(file_text(3).contains("fooAlphaBar"));
        assert!(file_text(1).contains("handle_request"));
    }

    #[test]
    fn patterns_follow_option_terminators() {
        let repo = Path::new("/r");
        let idx = Path::new("/i");
        for t in [
            Tool::Asc,
            Tool::RgWarm,
            Tool::GitGrepRev,
            Tool::GitGrepWorktree,
        ] {
            let a = argv(t, &tools(), &q(QueryKind::Literal, "p"), repo, idx).unwrap();
            let at = a.iter().position(|x| x == "p").unwrap();
            let sep = a.iter().position(|x| x == "-e" || x == "--").unwrap();
            assert!(sep < at, "{t:?}: {a:?}");
        }
        let a = argv(
            Tool::GitGrepRev,
            &tools(),
            &q(QueryKind::Word, "p"),
            repo,
            idx,
        )
        .unwrap();
        assert_eq!(&a[a.len() - 2..], ["master", "--"]);
        assert!(a.contains(&"-w".to_string()));
    }

    #[test]
    fn missing_tool_has_no_argv() {
        let mut t = tools();
        t.rg = None;
        assert!(
            argv(
                Tool::RgCold,
                &t,
                &q(QueryKind::Regex, "x"),
                Path::new("/r"),
                Path::new("/i")
            )
            .is_none()
        );
        let o = measure(Tool::RgCold, None, Path::new("."), 3);
        assert!(matches!(o, Outcome::Unsupported { .. }));
    }

    #[test]
    fn regex_for_rg_has_no_fixed_string_flag() {
        let a = argv(
            Tool::RgWarm,
            &tools(),
            &q(QueryKind::Regex, r"\bx\b"),
            Path::new("/r"),
            Path::new("/i"),
        )
        .unwrap();
        assert!(!a.contains(&"-F".to_string()) && !a.contains(&"-E".to_string()));
    }

    #[test]
    fn zoekt_query_escapes_quotes() {
        let a = argv(
            Tool::Zoekt,
            &tools(),
            &q(QueryKind::Literal, "a\"b"),
            Path::new("/r"),
            Path::new("/i"),
        )
        .unwrap();
        assert_eq!(a.last().unwrap(), "case:yes \"a\\\"b\"");
    }

    #[test]
    fn suites() {
        assert_eq!(suite("codeintel-quick").unwrap().len(), 2);
        assert_eq!(suite("codeintel-full").unwrap().len(), 3);
        assert!(suite("quick").is_none());
    }
}
