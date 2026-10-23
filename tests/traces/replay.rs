//! Turns a [`Trace`] into the shell script the compat runner runs on each side.
//!
//! The script starts with `git init` (the trace's first step), then replays every step in order.
//! A git step goes through `ascs`, which prints `$ git <args>` and `[exit N]` on both streams
//! (as the compat `g` function does) and appends `<label> <exit>` to `$LOG`, a file outside the
//! work tree. A step's hooks are written as scripts before the step and deleted after it, so a
//! hook that ran no git call in the recording does not exist in the replay.

use std::fmt::Write as _;

use super::model::{Call, Hook, Trace};

/// Defines `ascs <label> <git args…>`. Hook scripts carry the same text.
const ASCS: &str = r#"ascs() {
  label=$1
  shift
  printf '$ git %s\n' "$*"
  printf '$ git %s\n' "$*" >&2
  git "$@"
  rc=$?
  printf '[exit %s]\n' "$rc"
  printf '[exit %s]\n' "$rc" >&2
  echo "$label $rc" >> "$LOG"
  return 0
}"#;

/// The git directory the replaying side reports. An asc workspace keeps it outside `${ROOT}/.git`.
const GITDIR: &str = "$(git rev-parse --absolute-git-dir)";
const GITDIR_LIVE: &str = "\"$(git rev-parse --absolute-git-dir)\"";

/// Single-quotes `s` for `sh`, leaving `${ROOT}` and `${GITDIR}` live.
pub fn quote(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let root = rest
            .find("${ROOT}")
            .map(|at| (at, "${ROOT}".len(), "\"$ROOT\""));
        let gitdir = rest
            .find("${GITDIR}")
            .map(|at| (at, "${GITDIR}".len(), GITDIR_LIVE));
        let next = match (root, gitdir) {
            (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
            (a, b) => a.or(b),
        };
        let Some((at, len, live)) = next else { break };
        out.push('\'');
        out.push_str(&rest[..at].replace('\'', r"'\''"));
        out.push('\'');
        out.push_str(live);
        rest = &rest[at + len..];
    }
    out.push('\'');
    out.push_str(&rest.replace('\'', r"'\''"));
    out.push('\'');
    out
}

/// Shell text from a `sh` step: `${ROOT}` is a variable already; `${GITDIR}` becomes a substitution.
fn shell_text(s: &str) -> String {
    s.replace("${GITDIR}", GITDIR)
}

fn words(argv: &[String]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// `K='v' …` as `export` statements for a subshell.
fn exports(env: &std::collections::BTreeMap<String, String>) -> String {
    env.iter().fold(String::new(), |mut out, (k, v)| {
        let _ = write!(out, "export {k}={}; ", quote(v));
        out
    })
}

/// The labels the real-git side must log, in order: `s<i>` for a git step, `s<i>.<hook>.<j>`
/// for a git call inside a hook, each with the recorded exit code.
pub fn expected_exits(trace: &Trace) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    for (i, step) in trace.steps.iter().enumerate() {
        // Git runs a step's hooks before the step finishes, so their calls log first.
        for hook in &step.hooks {
            for (j, call) in hook.calls.iter().enumerate() {
                if call.git.is_some() {
                    out.push((
                        format!("s{}.{}.{}", i + 1, hook.name, j + 1),
                        call.exit.unwrap_or(0),
                    ));
                }
            }
        }
        if step.git.is_some() {
            out.push((format!("s{}", i + 1), step.exit.unwrap_or(0)));
        }
    }
    out
}

/// The argv words (joined by spaces) of the calls the shim hands to real git, in the order they
/// run (a step's hook calls, then the step); the asc side's `ASC_SHIM_TRACE` must list exactly these.
pub fn real_routed(trace: &Trace) -> Vec<String> {
    let mut out = Vec::new();
    for step in &trace.steps {
        for call in step.hooks.iter().flat_map(|h| &h.calls) {
            if let (Some(argv), Some(_)) = (&call.git, &call.routes) {
                out.push(argv.join(" "));
            }
        }
        if let (Some(argv), Some(_)) = (&step.git, &step.routes) {
            out.push(argv.join(" "));
        }
    }
    out
}

pub fn script(trace: &Trace) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ROOT=$(wpwd)\nLOG=\"$ROOT/../exits.log\"\nexport ROOT LOG\n: > \"$LOG\"\n{ASCS}"
    );
    for (i, step) in trace.steps.iter().enumerate() {
        let n = i + 1;
        let _ = writeln!(out, "# step {n} from {}", step.from.replace('\n', " "));
        if let Some(sh) = &step.sh {
            let _ = writeln!(out, "(\n{}\n)", shell_text(sh));
        } else if let Some(argv) = &step.git {
            for hook in &step.hooks {
                out.push_str(&hook_install(n, hook));
            }
            let _ = writeln!(
                out,
                "{}",
                git_line(&format!("s{n}"), argv, &step.env, step.stdin.as_deref())
            );
            for hook in &step.hooks {
                let _ = writeln!(out, "rm -f {}", quote(&hook.path));
            }
        }
        // hook_event steps pin a JSON contract for the asc hooks; git has nothing to replay.
    }
    out
}

fn git_line(
    label: &str,
    argv: &[String],
    env: &std::collections::BTreeMap<String, String>,
    stdin: Option<&str>,
) -> String {
    let call = format!("ascs {label} {}", words(argv));
    let piped = match stdin {
        Some(s) => format!("printf '%s' {} | {call}", quote(s)),
        None => call,
    };
    if env.is_empty() {
        piped
    } else {
        format!("( {}{piped} )", exports(env))
    }
}

fn hook_install(n: usize, hook: &Hook) -> String {
    let mut body = format!("#!/bin/sh\n{ASCS}\n");
    for (j, call) in hook.calls.iter().enumerate() {
        body.push_str(&hook_call(n, hook, j + 1, call));
    }
    let _ = writeln!(body, "exit {}", hook.exit);
    let path = quote(&hook.path);
    format!(
        "mkdir -p \"$(dirname {path})\"\ncat > {path} <<'ASC_TRACE_HOOK_EOF'\n{body}ASC_TRACE_HOOK_EOF\nchmod +x {path}\n"
    )
}

fn hook_call(n: usize, hook: &Hook, j: usize, call: &Call) -> String {
    if let Some(sh) = &call.sh {
        return format!("(\n{}\n)\n", shell_text(sh));
    }
    let argv = call.git.as_deref().unwrap_or_default();
    // The environment of a hook call is what git hands the hook; replaying it would mask a
    // difference, so it is not applied.
    format!(
        "{}\n",
        git_line(
            &format!("s{n}.{}.{j}", hook.name),
            argv,
            &Default::default(),
            None
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_keeps_root_live_and_escapes_quotes() {
        assert_eq!(quote("a'b"), r"'a'\''b'");
        assert_eq!(quote("${ROOT}/x y"), r#"''"$ROOT"'/x y'"#);
    }

    #[test]
    fn a_hook_exists_only_around_its_step() {
        let text = r#"
tool = "t"
version = "1"
provenance = "authored"
recapture = "S7"
git = "2.55.0"
driver = "d"
[[step]]
git = ["commit", "-m", "x"]
from = "driver"
exit = 0
[[step.hook]]
name = "pre-commit"
path = ".git/hooks/pre-commit"
exit = 0
[[step.hook.call]]
git = ["status", "--short"]
from = "t"
exit = 0
"#;
        let trace = Trace::parse("t", text).unwrap();
        let s = script(&trace);
        let install = s.find("cat > '.git/hooks/pre-commit'").unwrap();
        let run = s.find("ascs s1 'commit'").unwrap();
        let remove = s.find("rm -f '.git/hooks/pre-commit'").unwrap();
        assert!(install < run && run < remove);
        assert_eq!(
            expected_exits(&trace),
            [("s1.pre-commit.1".to_owned(), 0), ("s1".to_owned(), 0)]
        );
    }
}
