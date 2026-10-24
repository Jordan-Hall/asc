//! Running one generated script on both sides and classifying the result (PLAN Q04).
//!
//! The script runs once with the pinned oracle and once with the asc shim, under the compat
//! runner's environment (`fixed_env`, `at N` dates), and the two captures are compared by the
//! runner's own `compare`: stdout and stderr (which hold every command, its output and its exit
//! code, and the state read back after each step), the exit code, and the post-state (refs, HEAD,
//! `status --porcelain=v2 -z`, `ls-files -s`, the gitdir state files and the work tree). A
//! difference is dropped only by an `[[allow]]` entry of `allowed.toml`, or attributed to the
//! machine by the Q03 preflight. A command asc does not serve is a skip, not a failure.

use std::path::Path;

use regex::Regex;

use crate::harness::{Allow, Capture, Side, Tools};

/// The result of running one script.
#[derive(Debug)]
pub enum Outcome {
    /// Both sides agree, up to `allowed.toml`.
    Same,
    /// asc refused step `op` as unsupported (`fatal: asc: ... is not supported`); `command` is the
    /// git command it was running.
    Unserved { op: usize, command: String },
    /// A difference that no register entry covers.
    Diff(Diff),
    /// The preflight explains the difference: the product was not judged.
    Adapter(String),
    /// The harness could not run the case (a spawn failed, a time-out).
    Broken(String),
}

/// A difference between the oracle and asc.
#[derive(Debug, Clone)]
pub struct Diff {
    /// The differing sections, in report order.
    pub sections: Vec<String>,
    /// What the shrinker holds fixed: the first differing section and the command it came from.
    pub signature: String,
    /// The step the first difference belongs to, if it belongs to one (not a post-state section).
    pub op: Option<usize>,
    /// The runner's report: each differing section with the first lines that differ.
    pub report: String,
}

/// Both sides' captures of one script.
pub struct Pair {
    pub git: Capture,
    pub asc: Capture,
}

/// Runs `script` on both sides at once, each in its own directory under a fresh temp directory.
pub fn run_pair(tools: &Tools, script: &str) -> Result<Pair, String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let base: &Path = tmp.path();
    let (git, asc) = std::thread::scope(|scope| {
        let git = scope.spawn(|| tools.run_side(Side::Git, script, base, false));
        let asc = tools.run_side(Side::Asc, script, base, false);
        (
            git.join()
                .unwrap_or_else(|_| Err("the git side panicked".into())),
            asc,
        )
    });
    Ok(Pair { git: git?, asc: asc? })
}

/// Runs and classifies `script` as case `name` (the name selects the `allowed.toml` entries that
/// are not `scenario = "*"`). `tamper` alters the asc capture before the comparison (self-checks).
pub fn check(
    tools: &Tools,
    name: &str,
    script: &str,
    tamper: Option<&dyn Fn(&mut Capture)>,
) -> Outcome {
    let mut pair = match run_pair(tools, script) {
        Ok(p) => p,
        Err(e) => return Outcome::Broken(e),
    };
    if let Some(t) = tamper {
        t(&mut pair.asc);
    }
    classify(tools, name, &pair)
}

/// Classifies two captures.
pub fn classify(tools: &Tools, name: &str, pair: &Pair) -> Outcome {
    let (git, asc) = (&pair.git, &pair.asc);
    let stderr = asc.sections.get("stderr").map(Vec::as_slice).unwrap_or(b"");
    if let Some((op, command)) = unserved(&String::from_utf8_lossy(stderr)) {
        return Outcome::Unserved { op, command };
    }
    let allowed = match tools.allowed() {
        Ok(a) => a,
        Err(e) => return Outcome::Broken(e),
    };
    if !asc.fall_through.is_empty() {
        let report = format!(
            "{name}: the asc side fell through to real git inside the scenario:\n  {}\n",
            asc.fall_through.join("\n  ")
        );
        return Outcome::Diff(Diff {
            sections: vec!["fall-through".into()],
            signature: format!("fall-through|{}", asc.fall_through[0]),
            op: None,
            report,
        });
    }
    match crate::harness::compare(name, git, asc, &allowed) {
        Ok(()) => Outcome::Same,
        Err(report) => {
            let sections = sections_of(&report);
            let attributed = tools.attribute(name, &sections, report.clone());
            if attributed != report {
                return Outcome::Adapter(attributed);
            }
            let (signature, op) = signature(name, pair, &allowed, &sections);
            Outcome::Diff(Diff {
                sections,
                signature,
                op,
                report,
            })
        }
    }
}

/// The section names of a compare report (`--- <section>` at the start of a line).
pub fn sections_of(report: &str) -> Vec<String> {
    report
        .lines()
        .filter_map(|l| l.strip_prefix("--- "))
        .map(str::to_owned)
        .collect()
}

/// The step and git command that asc reported as unsupported, if its stderr says so. The stream
/// holds `[op N]` before each step and `$ git <args>` before each command.
pub fn unserved(stderr: &str) -> Option<(usize, String)> {
    let mut op = 0;
    let mut command = String::new();
    for line in stderr.lines() {
        if let Some(n) = line
            .strip_prefix("[op ")
            .and_then(|r| r.strip_suffix(']'))
            .and_then(|n| n.parse().ok())
        {
            op = n;
        } else if let Some(c) = line.strip_prefix("$ git ") {
            command = c.split(' ').next().unwrap_or_default().to_owned();
        } else if is_unserved_line(line) {
            return Some((op, command));
        }
    }
    None
}

fn is_unserved_line(line: &str) -> bool {
    let rest = line
        .strip_prefix("fatal: ")
        .or_else(|| line.strip_prefix("error: "));
    rest.is_some_and(|r| r.starts_with("asc: ") && r.contains("not supported"))
}

/// `bytes` as `compare` shows it (NUL as a line, the lines `allowed` drops removed).
fn visible(case: &str, section: &str, bytes: &[u8], allowed: &[Allow]) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(bytes)
        .replace('\0', "\n\u{2400}\n")
        .split('\n')
        .map(str::to_owned)
        .collect();
    for a in allowed {
        let section_ok = match a.section.strip_suffix('*') {
            Some(prefix) => section.starts_with(prefix),
            None => a.section == section,
        };
        if (a.scenario == "*" || a.scenario == case)
            && section_ok
            && let Ok(re) = Regex::new(&a.drop)
        {
            lines.retain(|l| !re.is_match(l));
        }
    }
    lines
}

/// The signature of a difference and the step it belongs to. The signature is the earliest
/// differing command of the transcript (`stdout` or `stderr`, then the command line), so that a
/// shrunken sequence is held to the same command; when only the post-state differs it is the
/// first differing post-state section.
fn signature(
    case: &str,
    pair: &Pair,
    allowed: &[Allow],
    sections: &[String],
) -> (String, Option<usize>) {
    let mut best: Option<(usize, String)> = None;
    for name in sections
        .iter()
        .filter(|s| matches!(s.as_str(), "stdout" | "stderr"))
    {
        let side = |c: &Capture| {
            c.sections
                .get(name)
                .map(|b| visible(case, name, b, allowed))
                .unwrap_or_default()
        };
        let (a, b) = (side(&pair.git), side(&pair.asc));
        let at = a
            .iter()
            .zip(b.iter())
            .position(|(x, y)| x != y)
            .unwrap_or_else(|| a.len().min(b.len()));
        let mut command = String::new();
        let mut op = 0;
        for line in a.iter().take(at + 1) {
            if let Some(c) = line.strip_prefix("$ git ") {
                command = c.to_owned();
            } else if let Some(n) = line
                .strip_prefix("[op ")
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|n| n.parse().ok())
            {
                op = n;
            }
        }
        if best.as_ref().is_none_or(|(o, _)| op < *o) {
            best = Some((op, format!("{name}|{command}")));
        }
    }
    if let Some((op, sig)) = best {
        return (sig, Some(op));
    }
    // Only the post-state differs: the file name is part of a tree section, the steps are not.
    let first = sections.first().map_or("", String::as_str);
    let kind = first.split_once("tree/").map_or(first, |_| "post:tree");
    (kind.to_owned(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_read_from_the_report() {
        let r = "case: asc differs\n--- stdout\n  -a\n  +b\n--- post:ls-files\n  -c\n  +d\n";
        assert_eq!(sections_of(r), ["stdout", "post:ls-files"]);
    }

    #[test]
    fn the_signature_is_the_earliest_differing_command() {
        let cap = |out: &str, err: &str, status: &str| {
            let mut c = Capture::default();
            c.sections.insert("stdout".into(), out.as_bytes().to_vec());
            c.sections.insert("stderr".into(), err.as_bytes().to_vec());
            c.sections
                .insert("post:status".into(), status.as_bytes().to_vec());
            c
        };
        let out = "[op 0]\n$ git add -A\n[exit 0]\n[op 1]\n$ git commit -m x\n[exit 0]\n";
        let pair = Pair {
            git: cap(out, "[op 0]\n$ git add -A\nfatal: no\n[exit 128]\n", "a"),
            asc: cap(
                &out.replace("commit -m x\n[exit 0]", "commit -m x\nextra\n[exit 0]"),
                "[op 0]\n$ git add -A\nfatal: nope\n[exit 128]\n",
                "b",
            ),
        };
        let sections = ["post:status".to_owned(), "stderr".into(), "stdout".into()];
        // The stderr difference is in step 0, the stdout one in step 1, the post-state is last.
        assert_eq!(
            signature("c", &pair, &[], &sections),
            ("stderr|add -A".to_owned(), Some(0))
        );
        let sections = ["post:status".to_owned(), "stdout".into()];
        assert_eq!(
            signature("c", &pair, &[], &sections),
            ("stdout|commit -m x".to_owned(), Some(1))
        );
        let sections = ["post:tree/a.txt".to_owned()];
        assert_eq!(
            signature("c", &pair, &[], &sections),
            ("post:tree".to_owned(), None)
        );
    }

    #[test]
    fn an_unsupported_command_is_found_with_its_step() {
        let stderr = "[op 0]\n$ git init\n[exit 0]\n[op 3]\nat\n$ git rebase -i b1\n\
                      fatal: asc: rebase --root is not supported\n[exit 128]\n";
        assert_eq!(unserved(stderr), Some((3, "rebase".into())));
        assert_eq!(
            unserved("[op 1]\n$ git add\nfatal: pathspec 'x' did not match\n"),
            None
        );
        // Only asc's own message counts, not git's.
        assert_eq!(unserved("fatal: this is not supported\n"), None);
    }
}
