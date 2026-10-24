//! The adapter-gap preflight (PLAN Q03, `docs/git-fidelity.md` "Classifying a failure").
//!
//! A scenario can fail because the machine, not asc, differs from what the suite assumes: a file
//! system that drops the exec bit, a different `sh`, a default branch other than `master`, a time
//! zone or locale that git honours differently, or an oracle that is not the pinned git. The
//! preflight probes each of these once per test process, on the same two sides (real git and the
//! asc shim) a scenario runs on, and records a [`Verdict`] per check. When a scenario fails, the
//! runner asks [`explanation`] whether a failed check explains a differing section; if it does,
//! the case fails as an adapter gap (`A-n`, `docs/adapter-gaps.md`) and the product is not judged.
//! Otherwise the failure is a product failure, as before.
//!
//! The checks run through the [`Probe`] trait, so a self-check can force a failure without
//! breaking the machine.

use super::Side;

/// The result of one probe command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    pub stdout: String,
    pub code: i32,
}

/// What the preflight needs from the runner: run a script on a side, and judge the oracle.
pub trait Probe {
    /// Runs `script` under `sh` on `side`, in a fresh empty directory, with the scenario
    /// environment and prelude (`g`, `at`, `wpwd`).
    fn run(&self, side: Side, script: &str) -> Result<Ran, String>;
    /// `Ok(detail)` when the oracle is acceptable for this run; `Err("adapter gap A-n: ...")`
    /// when `ASC_ORACLE_STRICT=1` and it is not the pinned build.
    fn oracle(&self) -> Result<String, String>;
}

/// One preflight check and what it found.
#[derive(Clone, Debug)]
pub struct Verdict {
    /// `exec-bit`, `shell`, `default-branch`, `time-zone`, `locale` or `oracle`.
    pub check: &'static str,
    pub ok: bool,
    pub detail: String,
    /// The adapter gap (`A-n`) a failure of this check is reported as.
    pub gap: &'static str,
}

impl Verdict {
    /// The one-line report of a failed check: `adapter gap A-6: exec-bit: <detail>`.
    pub fn line(&self) -> String {
        format!("adapter gap {}: {}: {}", self.gap, self.check, self.detail)
    }
}

/// A script written, made executable and run: the exec bit survives the file system and `sh`.
pub const EXEC_BIT_SCRIPT: &str =
    "printf '#!/bin/sh\\necho ran\\n' > probe.sh\nchmod +x probe.sh\n./probe.sh\n";
/// The shell's identity on the first line, then a fixed probe of the constructs the scenarios use:
/// `$(( ))`, `printf --`, `test`, here-documents (plain and quoted), command substitution with
/// parameter expansion, and `trap`.
pub const SHELL_SCRIPT: &str = r#"printf '%s\n' "${BASH_VERSION:+bash $BASH_VERSION}${KSH_VERSION:+ksh}${ZSH_VERSION:+zsh}"
a=$((2 + 3 * 4))
echo "arith=$a"
printf -- '%s|%s\n' -x y
if test -n "x" && test ! -e /nonexistent-asc-compat-probe; then echo test=ok; fi
cat <<EOF
heredoc $a
EOF
cat <<'EOF'
literal $a
EOF
x=$(printf abc)
echo "${x#a}"
trap 'echo trapped' EXIT
echo body
"#;
/// What [`SHELL_SCRIPT`] prints after its identity line.
pub const SHELL_GOLDEN: &str =
    "arith=14\n-x|y\ntest=ok\nheredoc 14\nliteral $a\nbc\nbody\ntrapped\n";
/// `git init` in the compat home names the default branch.
pub const DEFAULT_BRANCH_SCRIPT: &str = "git init -q d\ncd d\ngit symbolic-ref HEAD\n";
/// `at 0` renders `1700000000 +0000`, and git renders a local date in UTC.
pub const TIME_ZONE_SCRIPT: &str = "at 0\nprintf '%s\\n' \"$GIT_AUTHOR_DATE\"\ngit init -q d\ncd d\ngit commit -q --allow-empty -m t\ngit log -1 --format=%ad --date=local\n";
/// A message git translates prints untranslated.
pub const LOCALE_SCRIPT: &str = "git nosuch-asc-probe 2>&1\n";

pub const TIME_ZONE_GOLDEN: &str = "1700000000 +0000\nTue Nov 14 22:13:20 2023\n";
pub const LOCALE_GOLDEN: &str = "git: 'nosuch-asc-probe' is not a git command. See 'git --help'.\n";
pub const BRANCH_GOLDEN: &str = "refs/heads/master\n";

/// Runs every check.
pub fn run_all(p: &dyn Probe) -> Vec<Verdict> {
    vec![
        exec_bit(p),
        shell(p),
        default_branch(p),
        time_zone(p),
        locale(p),
        oracle(p),
    ]
}

fn verdict(check: &'static str, gap: &'static str, found: Result<String, String>) -> Verdict {
    match found {
        Ok(detail) => Verdict {
            check,
            ok: true,
            detail,
            gap,
        },
        Err(detail) => Verdict {
            check,
            ok: false,
            detail,
            gap,
        },
    }
}

/// Runs `script` on `side` and requires exactly `want` on stdout and exit 0.
fn expect(p: &dyn Probe, side: Side, script: &str, want: &str) -> Result<(), String> {
    let name = match side {
        Side::Git => "git side",
        Side::Asc => "asc side",
    };
    let ran = p.run(side, script).map_err(|e| format!("{name}: {e}"))?;
    if ran.code != 0 || ran.stdout != want {
        return Err(format!(
            "{name}: exit {}, printed {:?}, expected {:?}",
            ran.code, ran.stdout, want
        ));
    }
    Ok(())
}

fn exec_bit(p: &dyn Probe) -> Verdict {
    let found = [Side::Git, Side::Asc]
        .into_iter()
        .try_for_each(|s| expect(p, s, EXEC_BIT_SCRIPT, "ran\n"))
        .map(|()| "a script made executable runs on both sides".to_owned())
        .map_err(|e| format!("a script made executable did not run ({e})"));
    verdict("exec-bit", "A-6", found)
}

fn shell(p: &dyn Probe) -> Verdict {
    let found = p
        .run(Side::Git, SHELL_SCRIPT)
        .map_err(|e| format!("cannot run the probe: {e}"))
        .and_then(|ran| {
            let (identity, rest) = ran.stdout.split_once('\n').unwrap_or(("", ""));
            let identity = if identity.is_empty() {
                "not bash, ksh or zsh (dash or busybox)"
            } else {
                identity
            };
            if ran.code == 0 && rest == SHELL_GOLDEN {
                Ok(format!(
                    "sh is {identity}; the probe script matches its golden output"
                ))
            } else {
                Err(format!(
                    "sh is {identity}; the probe script printed {rest:?} (exit {}), the golden \
                     output is {SHELL_GOLDEN:?}",
                    ran.code
                ))
            }
        });
    verdict("shell", "A-5", found)
}

fn default_branch(p: &dyn Probe) -> Verdict {
    let found = [Side::Git, Side::Asc]
        .into_iter()
        .try_for_each(|s| expect(p, s, DEFAULT_BRANCH_SCRIPT, BRANCH_GOLDEN))
        .map(|()| "`git init` in the compat home gives master on both sides".to_owned())
        .map_err(|e| format!("`git init` did not give master ({e})"));
    verdict("default-branch", "A-6", found)
}

fn time_zone(p: &dyn Probe) -> Verdict {
    let found = expect(p, Side::Git, TIME_ZONE_SCRIPT, TIME_ZONE_GOLDEN)
        .map(|()| "`at 0` renders 1700000000 +0000 and a local date renders in UTC".to_owned())
        .map_err(|e| format!("the clock is not fixed to UTC ({e})"));
    verdict("time-zone", "A-6", found)
}

fn locale(p: &dyn Probe) -> Verdict {
    // The script's exit code is git's (1); only the message is checked.
    let found = p
        .run(Side::Git, LOCALE_SCRIPT)
        .map_err(|e| format!("cannot run the probe: {e}"))
        .and_then(|ran| {
            if ran.stdout == LOCALE_GOLDEN {
                Ok("git messages are untranslated (LC_ALL=C)".to_owned())
            } else {
                Err(format!(
                    "git printed {:?}, expected {LOCALE_GOLDEN:?}; the scenarios compare \
                     English messages",
                    ran.stdout
                ))
            }
        });
    verdict("locale", "A-6", found)
}

fn oracle(p: &dyn Probe) -> Verdict {
    match p.oracle() {
        Ok(detail) => Verdict {
            check: "oracle",
            ok: true,
            detail,
            gap: "A-3",
        },
        Err(e) => Verdict {
            check: "oracle",
            ok: false,
            gap: if e.contains("A-4") { "A-4" } else { "A-3" },
            detail: e
                .split_once(": ")
                .filter(|(head, _)| head.starts_with("adapter gap"))
                .map_or(e.clone(), |(_, rest)| rest.to_owned()),
        },
    }
}

/// True when a failed `check` can explain a difference in `section`. The locale check explains
/// only the message streams; every other check can change any section.
pub fn explains(check: &str, section: &str) -> bool {
    match check {
        "locale" => section == "stdout" || section == "stderr",
        _ => true,
    }
}

/// The first failed check that explains one of the differing `sections`, if any.
pub fn explanation<'a>(verdicts: &'a [Verdict], sections: &[String]) -> Option<&'a Verdict> {
    verdicts
        .iter()
        .filter(|v| !v.ok)
        .find(|v| sections.iter().any(|s| explains(v.check, s)))
}

/// The failure text of a case an adapter gap explains. `report` is the comparison that failed.
pub fn adapter_failure(case: &str, v: &Verdict, report: &str) -> String {
    format!(
        "{case}: {}\n  This is an adapter problem, not a product failure: the product was not \
         judged. Fix the environment (docs/adapter-gaps.md {}) and run the case again.\n\
         The comparison that failed:\n{report}",
        v.line(),
        v.gap
    )
}
